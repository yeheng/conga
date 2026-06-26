//! JSON file-backed event store — the SQLite-free backend.
//!
//! Layout under `base` (typically `~/.gasket`):
//! - `sessions/<channel>/<chat_id>.jsonl` — one event per line (the append log)
//! - `sessions/<channel>/<chat_id>.meta.json` — per-session sidecar (summary,
//!   watermark, checkpoints, compaction state)
//! - `state/maintenance.json` — per-(task, session) maintenance watermarks
//!
//! ## Invariants (enforced by every write path)
//! 1. **File content**: a `.jsonl` only holds events with `sequence >
//!    meta.watermark`; compaction rewrites the file to drop the GC'd prefix.
//! 2. **Monotonic sequence**: `next_sequence` is rebuilt from the file
//!    (`max(seq)+1`) on cache miss, never reused or rolled back.
//! 3. **Write serialization**: all writes for one session go through one
//!    per-session `tokio::sync::Mutex`; reads take no lock and rely on atomic
//!    file replacement for consistency.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use gasket_types::{ChannelType, SessionEvent, SessionKey};
use parking_lot::Mutex as PlMutex;
use tokio::sync::{broadcast, Mutex as TokioMutex};
use tracing::warn;

use crate::fs::atomic_write;
use crate::store_trait::{EventStoreTrait, SessionStoreTrait, StoreError};

/// Per-session sidecar metadata, persisted as `<chat_id>.meta.json`.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct SessionMeta {
    #[serde(default)]
    summary: String,
    #[serde(default)]
    watermark: i64,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    updated_at: String,
    #[serde(default)]
    checkpoints: Vec<Checkpoint>,
    #[serde(default)]
    compaction_in_progress: bool,
    #[serde(default)]
    compaction_started_at: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct Checkpoint {
    target_sequence: i64,
    summary: String,
    created_at: String,
}

/// JSON file-backed implementation of both [`EventStoreTrait`] and
/// [`SessionStoreTrait`].
///
/// Shared as a single `Arc<JsonStore>` across the event-store and
/// session-store roles (see `bootstrap`) so the per-session write locks are
/// global — this is what makes cross-field writes serialize correctly.
pub struct JsonStore {
    base: PathBuf,
    tx: broadcast::Sender<SessionEvent>,
    /// Per-session write locks, lazily created.
    locks: PlMutex<HashMap<String, Arc<TokioMutex<()>>>>,
    /// Cached `next_sequence` per session (invalidated on GC / clear).
    next_seq: PlMutex<HashMap<String, i64>>,
}

/// Sanitize a session-key component for use as a single path segment.
fn safe_name(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '\0' | '"' | '\n' | '\r' => '_',
            _ => c,
        })
        .collect()
}

impl JsonStore {
    pub fn new(base: PathBuf) -> Self {
        let (tx, _) = broadcast::channel(64);
        Self {
            base,
            tx,
            locks: PlMutex::new(HashMap::new()),
            next_seq: PlMutex::new(HashMap::new()),
        }
    }

    fn channel_dir(&self, key: &SessionKey) -> PathBuf {
        self.base
            .join("sessions")
            .join(safe_name(&key.channel.to_string()))
    }
    fn events_path(&self, key: &SessionKey) -> PathBuf {
        self.channel_dir(key)
            .join(format!("{}.jsonl", safe_name(&key.chat_id)))
    }
    fn meta_path(&self, key: &SessionKey) -> PathBuf {
        self.channel_dir(key)
            .join(format!("{}.meta.json", safe_name(&key.chat_id)))
    }
    fn lock_key(key: &SessionKey) -> String {
        key.to_string()
    }

    /// Get (or lazily create) the per-session write lock.
    fn write_lock(&self, key: &SessionKey) -> Arc<TokioMutex<()>> {
        let k = Self::lock_key(key);
        self.locks
            .lock()
            .entry(k)
            .or_insert_with(|| Arc::new(TokioMutex::new(())))
            .clone()
    }

    /// Read all events from a session file, skipping unparseable trailing lines.
    fn read_events(path: &Path) -> Vec<SessionEvent> {
        let Ok(text) = std::fs::read_to_string(path) else {
            return vec![];
        };
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| match serde_json::from_str::<SessionEvent>(l) {
                Ok(e) => Some(e),
                Err(_) => {
                    warn!(line = l, "Skipping unparseable event line");
                    None
                }
            })
            .collect()
    }

    fn read_meta(&self, key: &SessionKey) -> SessionMeta {
        let p = self.meta_path(key);
        std::fs::read_to_string(&p)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }
    async fn write_meta(&self, key: &SessionKey, meta: &SessionMeta) -> Result<(), StoreError> {
        let dir = self.channel_dir(key);
        tokio::fs::create_dir_all(&dir).await?;
        let json = serde_json::to_string_pretty(meta)?;
        atomic_write(&self.meta_path(key), json).await?;
        Ok(())
    }

    /// Rebuild `next_sequence` and cache it.
    ///
    /// `next_sequence = max(seq in file) + 1`, but never below the summary
    /// watermark. Including the watermark is essential after a full compaction
    /// GC: the event file is then empty (all events at or below the watermark
    /// were discarded), but the watermark still records how far the sequence
    /// counter has progressed. Without it, a fresh append would restart from 0
    /// and violate monotonicity (and confuse `get_events_after_sequence`).
    ///
    /// A `watermark == 0` means "no summary yet" (the default) and does not
    /// raise the floor — otherwise a brand-new session's first event would
    /// start at sequence 1 instead of 0.
    fn rebuild_next_seq(&self, key: &SessionKey) -> i64 {
        let events = Self::read_events(&self.events_path(key));
        let file_max = events.iter().map(|e| e.sequence).max().unwrap_or(-1);
        let meta = self.read_meta(key);
        let wm_floor = if meta.watermark > 0 {
            meta.watermark
        } else {
            -1
        };
        let next = file_max.max(wm_floor) + 1;
        self.next_seq.lock().insert(Self::lock_key(key), next);
        next
    }
    fn cached_next_seq(&self, key: &SessionKey) -> i64 {
        let k = Self::lock_key(key);
        if let Some(&n) = self.next_seq.lock().get(&k) {
            return n;
        }
        self.rebuild_next_seq(key)
    }

    /// Walk every session file across all channels, invoking `f` with each
    /// session's full event list.
    fn for_each_sessions_dir(&self, mut f: impl FnMut(Vec<SessionEvent>)) {
        let root = self.base.join("sessions");
        let Ok(channels) = std::fs::read_dir(&root) else {
            return;
        };
        for ch in channels.flatten() {
            let Ok(files) = std::fs::read_dir(ch.path()) else {
                continue;
            };
            for file in files.flatten() {
                let path = file.path();
                if path.extension().and_then(|s| s.to_str()) == Some("jsonl") {
                    f(Self::read_events(&path));
                }
            }
        }
    }
}

#[async_trait]
impl EventStoreTrait for JsonStore {
    async fn append(&self, event: &SessionEvent) -> Result<i64, StoreError> {
        let key = SessionKey::parse(&event.session_key)
            .unwrap_or_else(|| SessionKey::new(ChannelType::Cli, &event.session_key));
        let lock = self.write_lock(&key);
        let _g = lock.lock().await;

        let dir = self.channel_dir(&key);
        tokio::fs::create_dir_all(&dir).await?;

        let seq = self.cached_next_seq(&key);
        let mut ev = event.clone();
        ev.sequence = seq;

        let line = serde_json::to_string(&ev)?;
        let path = self.events_path(&key);
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .await?;
        use tokio::io::AsyncWriteExt;
        file.write_all(line.as_bytes()).await?;
        file.write_all(b"\n").await?;
        file.flush().await?;

        // Bump cached counter.
        self.next_seq.lock().insert(Self::lock_key(&key), seq + 1);

        // Touch meta `updated_at` (create the sidecar if missing).
        let mut meta = self.read_meta(&key);
        if meta.created_at.is_empty() {
            meta.created_at = Utc::now().to_rfc3339();
        }
        meta.updated_at = Utc::now().to_rfc3339();
        self.write_meta(&key, &meta).await?;

        let _ = self.tx.send(ev.clone());
        Ok(seq)
    }

    async fn get_session_history(&self, key: &SessionKey) -> Result<Vec<SessionEvent>, StoreError> {
        let mut v = Self::read_events(&self.events_path(key));
        v.sort_by_key(|e| e.sequence);
        Ok(v)
    }

    async fn get_events_after_sequence(
        &self,
        key: &SessionKey,
        after: i64,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        let mut v: Vec<_> = Self::read_events(&self.events_path(key))
            .into_iter()
            .filter(|e| e.sequence > after)
            .collect();
        v.sort_by_key(|e| e.sequence);
        Ok(v)
    }

    async fn get_events_up_to_sequence(
        &self,
        key: &SessionKey,
        target: i64,
    ) -> Result<Vec<SessionEvent>, StoreError> {
        let mut v: Vec<_> = Self::read_events(&self.events_path(key))
            .into_iter()
            .filter(|e| e.sequence <= target && !e.event_type.is_summary())
            .collect();
        v.sort_by_key(|e| e.sequence);
        Ok(v)
    }

    async fn get_max_sequence(&self, key: &SessionKey) -> Result<i64, StoreError> {
        Ok(Self::read_events(&self.events_path(key))
            .iter()
            .map(|e| e.sequence)
            .max()
            .unwrap_or(0))
    }

    async fn get_event_ids_up_to(
        &self,
        key: &SessionKey,
        up_to: i64,
    ) -> Result<Vec<String>, StoreError> {
        Ok(Self::read_events(&self.events_path(key))
            .into_iter()
            .filter(|e| e.sequence <= up_to)
            .map(|e| e.id.to_string())
            .collect())
    }

    async fn delete_events_upto(&self, key: &SessionKey, target: i64) -> Result<u64, StoreError> {
        let lock = self.write_lock(key);
        let _g = lock.lock().await;

        let path = self.events_path(key);
        let events = Self::read_events(&path);
        let deleted = events.iter().filter(|e| e.sequence <= target).count() as u64;
        let kept: Vec<&SessionEvent> = events.iter().filter(|e| e.sequence > target).collect();

        // Rewrite the file: concatenate kept lines, atomically replace.
        let mut buf = String::new();
        for e in &kept {
            buf.push_str(&serde_json::to_string(e)?);
            buf.push('\n');
        }
        atomic_write(&path, buf).await?;

        // Invalidate the next_sequence cache — the file changed.
        self.next_seq.lock().remove(&Self::lock_key(key));
        Ok(deleted)
    }

    async fn get_events_by_ids_global(
        &self,
        ids: &[uuid::Uuid],
    ) -> Result<Vec<SessionEvent>, StoreError> {
        let want: std::collections::HashSet<uuid::Uuid> = ids.iter().copied().collect();
        let mut out = vec![];
        self.for_each_sessions_dir(|events| {
            out.extend(events.into_iter().filter(|e| want.contains(&e.id)));
        });
        out.sort_by(|a, b| {
            a.session_key
                .cmp(&b.session_key)
                .then(a.sequence.cmp(&b.sequence))
        });
        Ok(out)
    }

    async fn get_recent_events(&self, limit: usize) -> Result<Vec<SessionEvent>, StoreError> {
        let mut all = vec![];
        self.for_each_sessions_dir(|events| {
            all.extend(events.into_iter().filter(|e| {
                matches!(
                    e.event_type,
                    gasket_types::EventType::UserMessage
                        | gasket_types::EventType::AssistantMessage
                )
            }));
        });
        all.sort_by(|a, b| {
            a.session_key
                .cmp(&b.session_key)
                .then(a.sequence.cmp(&b.sequence))
        });
        if limit > 0 {
            all.truncate(limit);
        }
        Ok(all)
    }

    async fn clear_session(&self, key: &SessionKey) -> Result<(), StoreError> {
        let lock = self.write_lock(key);
        let _g = lock.lock().await;
        let _ = tokio::fs::remove_file(self.events_path(key)).await;
        let _ = tokio::fs::remove_file(self.meta_path(key)).await;
        self.next_seq.lock().remove(&Self::lock_key(key));
        Ok(())
    }

    fn subscribe(&self) -> broadcast::Receiver<SessionEvent> {
        self.tx.subscribe()
    }
}

#[async_trait]
impl SessionStoreTrait for JsonStore {
    async fn save_summary(
        &self,
        key: &SessionKey,
        content: &str,
        watermark: i64,
    ) -> Result<(), StoreError> {
        let lock = self.write_lock(key);
        let _g = lock.lock().await;
        let mut meta = self.read_meta(key);
        meta.summary = content.to_string();
        meta.watermark = watermark;
        meta.updated_at = Utc::now().to_rfc3339();
        self.write_meta(key, &meta).await
    }

    async fn load_summary(&self, key: &SessionKey) -> Result<Option<(String, i64)>, StoreError> {
        let m = self.read_meta(key);
        if m.summary.is_empty() && m.watermark == 0 {
            Ok(None)
        } else {
            Ok(Some((m.summary, m.watermark)))
        }
    }

    async fn delete_summary(&self, key: &SessionKey) -> Result<bool, StoreError> {
        let lock = self.write_lock(key);
        let _g = lock.lock().await;
        let mut meta = self.read_meta(key);
        let had = !meta.summary.is_empty();
        meta.summary.clear();
        meta.watermark = 0;
        self.write_meta(key, &meta).await?;
        Ok(had)
    }

    async fn load_summary_with_checkpoint(
        &self,
        key: &SessionKey,
    ) -> Result<(String, String, i64), StoreError> {
        let m = self.read_meta(key);
        let summary = m.summary.clone();
        let watermark = m.watermark;
        // Most recent checkpoint (latest target_sequence).
        let checkpoint = m
            .checkpoints
            .iter()
            .max_by_key(|c| c.target_sequence)
            .map(|c| c.summary.clone())
            .unwrap_or_default();
        Ok((summary, checkpoint, watermark))
    }

    async fn save_checkpoint(
        &self,
        key: &SessionKey,
        target: i64,
        summary: &str,
    ) -> Result<(), StoreError> {
        let lock = self.write_lock(key);
        let _g = lock.lock().await;
        let mut meta = self.read_meta(key);
        meta.checkpoints.retain(|c| c.target_sequence != target);
        meta.checkpoints.push(Checkpoint {
            target_sequence: target,
            summary: summary.to_string(),
            created_at: Utc::now().to_rfc3339(),
        });
        self.write_meta(key, &meta).await
    }

    async fn scan_active_sessions(&self) -> Result<Vec<(String, i64, String)>, StoreError> {
        let mut out = vec![];
        self.for_each_sessions_dir(|events| {
            if let Some(first) = events.first() {
                out.push((
                    first.session_key.clone(),
                    events.len() as i64,
                    first.created_at.to_rfc3339(),
                ));
            }
        });
        Ok(out)
    }

    async fn mark_compaction_started(&self, key: &SessionKey) -> Result<(), StoreError> {
        let lock = self.write_lock(key);
        let _g = lock.lock().await;
        let mut meta = self.read_meta(key);
        meta.compaction_in_progress = true;
        meta.compaction_started_at = Some(Utc::now().to_rfc3339());
        self.write_meta(key, &meta).await
    }

    async fn mark_compaction_finished(&self, key: &SessionKey) -> Result<(), StoreError> {
        let lock = self.write_lock(key);
        let _g = lock.lock().await;
        let mut meta = self.read_meta(key);
        meta.compaction_in_progress = false;
        meta.compaction_started_at = None;
        self.write_meta(key, &meta).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gasket_types::{EventMetadata, EventType};

    fn test_event(key: &SessionKey, seq: i64) -> SessionEvent {
        SessionEvent {
            id: uuid::Uuid::now_v7(),
            session_key: key.to_string(),
            event_type: EventType::UserMessage,
            content: "hi".to_string(),
            metadata: EventMetadata::default(),
            created_at: Utc::now(),
            sequence: seq,
        }
    }

    #[tokio::test]
    async fn test_meta_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonStore::new(dir.path().to_path_buf());
        let key = SessionKey::parse("cli:test").unwrap();
        let mut meta = SessionMeta::default();
        meta.summary = "s".into();
        meta.watermark = 5;
        store.write_meta(&key, &meta).await.unwrap();
        let loaded = store.read_meta(&key);
        assert_eq!(loaded.summary, "s");
        assert_eq!(loaded.watermark, 5);
    }

    #[tokio::test]
    async fn test_append_assigns_monotonic_seq() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonStore::new(dir.path().to_path_buf());
        let mk = |seq_drop: i64| SessionEvent {
            id: uuid::Uuid::now_v7(),
            session_key: "cli:s".into(),
            event_type: EventType::UserMessage,
            content: "hi".into(),
            metadata: EventMetadata::default(),
            created_at: Utc::now(),
            sequence: seq_drop,
        };
        assert_eq!(store.append(&mk(0)).await.unwrap(), 0);
        assert_eq!(store.append(&mk(0)).await.unwrap(), 1);
        // Incoming sequence is overwritten by the store-assigned monotonic one.
        assert_eq!(store.append(&mk(9)).await.unwrap(), 2);
    }

    #[tokio::test]
    async fn test_history_and_after_watermark() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonStore::new(dir.path().to_path_buf());
        let key = SessionKey::parse("cli:s").unwrap();
        for i in 0..5 {
            let mut e = test_event(&key, i);
            e.content = format!("m{i}");
            store.append(&e).await.unwrap();
        }
        let all = store.get_session_history(&key).await.unwrap();
        assert_eq!(all.len(), 5);
        assert_eq!(all[0].content, "m0");
        let after = store.get_events_after_sequence(&key, 2).await.unwrap();
        assert_eq!(after.len(), 2);
    }

    #[tokio::test]
    async fn test_gc_drops_prefix_and_preserves_seq_monotonicity() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonStore::new(dir.path().to_path_buf());
        let key = SessionKey::parse("cli:s").unwrap();
        for _ in 0..5 {
            store.append(&test_event(&key, 0)).await.unwrap();
        } // seq 0..4
        let deleted = store.delete_events_upto(&key, 2).await.unwrap();
        assert_eq!(deleted, 3);
        let kept = store.get_session_history(&key).await.unwrap();
        assert_eq!(kept.len(), 2);
        assert_eq!(kept[0].sequence, 3);
        // Subsequent append continues, never rolls back or reuses.
        let next = store.append(&test_event(&key, 0)).await.unwrap();
        assert_eq!(next, 5);
    }

    #[tokio::test]
    async fn test_summary_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonStore::new(dir.path().to_path_buf());
        let key = SessionKey::parse("cli:s").unwrap();
        assert_eq!(store.load_summary(&key).await.unwrap(), None);
        store.save_summary(&key, "sum", 3).await.unwrap();
        assert_eq!(
            store.load_summary(&key).await.unwrap(),
            Some(("sum".into(), 3))
        );
        assert!(store.delete_summary(&key).await.unwrap());
        assert_eq!(store.load_summary(&key).await.unwrap(), None);
    }

    /// End-to-end compaction cycle: append → save_summary → GC → append.
    /// Validates all three invariants hold together (file content, monotonic
    /// sequence, watermark consistency after GC).
    #[tokio::test]
    async fn test_compaction_cycle_append_summary_gc_append() {
        let dir = tempfile::tempdir().unwrap();
        let store = JsonStore::new(dir.path().to_path_buf());
        let key = SessionKey::parse("cli:cyc").unwrap();
        for _ in 0..10 {
            store.append(&test_event(&key, 0)).await.unwrap();
        } // seq 0..9

        // Compaction: record summary at watermark 5, then GC events <= 5.
        store.save_summary(&key, "summary", 5).await.unwrap();
        let deleted = store.delete_events_upto(&key, 5).await.unwrap();
        assert_eq!(deleted, 6); // seq 0..5 inclusive

        // Invariant 1: file only holds seq > watermark.
        let kept = store.get_session_history(&key).await.unwrap();
        assert!(kept.iter().all(|e| e.sequence > 5));
        assert_eq!(kept.len(), 4); // seq 6..9

        // Watermark survived GC (set only by save_summary).
        assert_eq!(
            store.load_summary(&key).await.unwrap(),
            Some(("summary".into(), 5))
        );

        // Invariant 2: next append continues monotonically, no reuse/rollback.
        let next = store.append(&test_event(&key, 0)).await.unwrap();
        assert_eq!(next, 10);
    }
}
