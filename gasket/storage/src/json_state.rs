//! Tiny JSON-backed state files for cron / kv / maintenance.
//!
//! These replace three small SQLite tables (`cron_state`, `kv_store`,
//! `maintenance_state`) with one JSON file each under `<base>/state/`. Each
//! file is a serialized map mutated via read-modify-write under an atomic
//! temp-file + rename ([`crate::fs::atomic_write`]).
//!
//! Coexists with the SQLite small-table stores during Phase 2; the JSON path
//! becomes the default once SQLite is removed.

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

/// Generic key/value state: `{key: value}`.
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct KvStateFile {
    #[serde(default)]
    pub entries: HashMap<String, String>,
}

impl KvStateFile {
    pub async fn read(path: &Path, key: &str) -> anyhow::Result<Option<String>> {
        Ok(Self::load(path).await?.entries.get(key).cloned())
    }

    pub async fn write(path: &Path, key: &str, value: &str) -> anyhow::Result<()> {
        rmw(path, |s: &mut Self| {
            s.entries.insert(key.into(), value.into());
        })
        .await
    }

    pub async fn delete(path: &Path, key: &str) -> anyhow::Result<bool> {
        rmw(path, |s: &mut Self| s.entries.remove(key).is_some()).await
    }

    async fn load(path: &Path) -> anyhow::Result<Self> {
        Ok(tokio::fs::read_to_string(path)
            .await
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default())
    }
}

// ── maintenance_state ───────────────────────────────────────────────────────

/// Per-(task, session) watermark state, keyed by `"<task>\u{1f}<session>"`.
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct MaintenanceStateFile {
    #[serde(default)]
    pub watermarks: HashMap<String, i64>,
}

impl MaintenanceStateFile {
    /// Compound key for a (task, target-session) pair.
    fn mk(task: &str, target: &str) -> String {
        format!("{}\u{1f}{}", task, target)
    }

    /// In-memory watermark lookup on an already-loaded file (no IO).
    pub fn get_watermark(&self, task: &str, target: &str) -> i64 {
        self.watermarks
            .get(&Self::mk(task, target))
            .copied()
            .unwrap_or(0)
    }

    pub async fn read(path: &Path, task: &str, target: &str) -> anyhow::Result<i64> {
        Ok(Self::load(path).await?.get_watermark(task, target))
    }

    pub async fn write(path: &Path, task: &str, target: &str, wm: i64) -> anyhow::Result<()> {
        rmw(path, |s: &mut Self| {
            s.watermarks.insert(Self::mk(task, target), wm);
        })
        .await
    }

    async fn load(path: &Path) -> anyhow::Result<Self> {
        Ok(tokio::fs::read_to_string(path)
            .await
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_kv_json() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("kv.json");
        assert_eq!(KvStateFile::read(&p, "k").await.unwrap(), None);
        KvStateFile::write(&p, "k", "v").await.unwrap();
        assert_eq!(KvStateFile::read(&p, "k").await.unwrap(), Some("v".into()));
        assert!(KvStateFile::delete(&p, "k").await.unwrap());
        assert_eq!(KvStateFile::read(&p, "k").await.unwrap(), None);
    }

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

    #[tokio::test]
    async fn test_maintenance_json() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("maintenance.json");
        assert_eq!(
            MaintenanceStateFile::read(&p, "evolve", "cli:s")
                .await
                .unwrap(),
            0
        );
        MaintenanceStateFile::write(&p, "evolve", "cli:s", 7)
            .await
            .unwrap();
        assert_eq!(
            MaintenanceStateFile::read(&p, "evolve", "cli:s")
                .await
                .unwrap(),
            7
        );
    }
}
