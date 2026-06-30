//! Tiny JSON-backed state files for cron schedule state.
//!
//! Replaces the small SQLite `cron_state` table with one JSON file under
//! `<base>/state/`. Mutated via read-modify-write under an atomic temp-file
//! + rename ([`crate::fs::atomic_write`]).

use std::collections::HashMap;
use std::path::Path;

use crate::fs::atomic_write;

/// Read-modify-write a JSON-serialized value under `path`.
///
/// Loads (or defaults), applies `f`, then atomically persists the result.
async fn rmw<T, F, R>(path: &Path, f: F) -> anyhow::Result<R>
where
    T: serde::Serialize + serde::de::DeserializeOwned + Default,
    F: FnOnce(&mut T) -> R,
{
    let mut val: T = tokio::fs::read_to_string(path)
        .await
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let r = f(&mut val);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    atomic_write(path, serde_json::to_string_pretty(&val)?).await?;
    Ok(r)
}

// ── cron_state ──────────────────────────────────────────────────────────────

/// Cron job schedule state: `{job_id: {last_run_at, next_run_at}}`.
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct CronStateFile {
    #[serde(default)]
    pub jobs: HashMap<String, CronEntry>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct CronEntry {
    pub last_run_at: Option<String>,
    pub next_run_at: Option<String>,
}

impl CronStateFile {
    pub async fn get(
        path: &Path,
        job_id: &str,
    ) -> anyhow::Result<Option<(Option<String>, Option<String>)>> {
        let map = Self::load(path).await?;
        Ok(map
            .jobs
            .get(job_id)
            .map(|e| (e.last_run_at.clone(), e.next_run_at.clone())))
    }

    pub async fn upsert(
        path: &Path,
        job_id: &str,
        last: Option<&str>,
        next: Option<&str>,
    ) -> anyhow::Result<()> {
        rmw(path, |s: &mut Self| {
            s.jobs.insert(
                job_id.into(),
                CronEntry {
                    last_run_at: last.map(Into::into),
                    next_run_at: next.map(Into::into),
                },
            );
        })
        .await
    }

    pub async fn delete(path: &Path, job_id: &str) -> anyhow::Result<bool> {
        rmw(path, |s: &mut Self| s.jobs.remove(job_id).is_some()).await
    }

    async fn load(path: &Path) -> anyhow::Result<Self> {
        Ok(tokio::fs::read_to_string(path)
            .await
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default())
    }
}

// ── kv_store ────────────────────────────────────────────────────────────────
// Removed: generic KV store was only used by WorkflowTool checkpoint/recovery,
// which has been deleted. Cron schedule state above is the only remaining consumer.

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_cron_json() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("cron.json");
        assert_eq!(CronStateFile::get(&p, "job").await.unwrap(), None);
        CronStateFile::upsert(&p, "job", Some("now"), Some("later"))
            .await
            .unwrap();
        let got = CronStateFile::get(&p, "job").await.unwrap().unwrap();
        assert_eq!(got.0.as_deref(), Some("now"));
        assert_eq!(got.1.as_deref(), Some("later"));
        assert!(CronStateFile::delete(&p, "job").await.unwrap());
    }
}
