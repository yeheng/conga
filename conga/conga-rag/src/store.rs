//! Store: documents + chunks in SQLite, embeddings in a qdrant-edge shard
//! (in-process vector engine) living next to the db file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use anyhow::Context;
use qdrant_edge::external::serde_json::json;
use qdrant_edge::{
    Condition, CountRequest, Distance, EdgeConfig, EdgeShard, EdgeVectorParams, FieldCondition,
    Filter, Match, NamedQuery, PointId, PointInsertOperations, PointOperations, PointStruct,
    QueryEnum, SearchRequestBuilder, UpdateOperation,
};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};

/// Search hit, `score` = cosine similarity (higher is better).
#[derive(Debug, Clone)]
pub struct Hit {
    pub source: String,
    pub path: String,
    pub ordinal: usize,
    pub content: String,
    pub score: f64,
}

#[derive(Debug, Clone)]
pub struct DocRow {
    pub path: PathBuf,
    pub mtime: i64,
    pub content_hash: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SourceStat {
    pub source: String,
    pub docs: i64,
    pub chunks: i64,
}

/// Process-wide live stores keyed by db path. qdrant-edge holds an
/// exclusive WAL lock per shard dir for the lifetime of the shard
/// instance, so two `Store`s on the same path cannot coexist — the second
/// open fails with `Can't init WAL: WouldBlock`. In-process callers
/// (rag_search / rag_remember / the evolve hook each open the store per
/// call) share one instance per path through this registry.
static REGISTRY: OnceLock<Mutex<HashMap<PathBuf, Weak<StoreShared>>>> = OnceLock::new();
/// Serializes first-instance creation so concurrent first-opens don't
/// race on the edge WAL lock.
static OPEN_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Cheap handle over the shared per-path instance.
pub struct Store {
    inner: Arc<StoreShared>,
}

struct StoreShared {
    pool: SqlitePool,
    /// Vector index; `None` until `ensure_vec` creates it (the embedding
    /// dimension is only known at first ingest). Behind an async mutex
    /// because `ensure_vec` installs it exclusively.
    shard: tokio::sync::Mutex<Option<EdgeShard>>,
    /// Db file path — the edge shard directory is derived from it.
    db_path: PathBuf,
}

/// Persisted qdrant-edge shard manifest (kept in sync with the crate's
/// `EDGE_CONFIG_FILE`, which is `pub(crate)` there).
const EDGE_CONFIG_FILE: &str = "edge_config.json";

/// Edge shard directory for a db path: `<db>.edge/` sibling. Public for
/// the pipeline's `--rebuild` cleanup.
pub fn edge_dir(db: &Path) -> PathBuf {
    let mut s = db.as_os_str().to_os_string();
    s.push(".edge");
    PathBuf::from(s)
}

/// Keyword-equality filter on one payload field (values are bound, never
/// interpolated into SQL).
fn kw_filter(field: &str, value: &str) -> Filter {
    Filter {
        should: None,
        min_should: None,
        must: Some(vec![Condition::Field(FieldCondition::new_match(
            field.try_into().expect("static field name"),
            Match::from(value.to_string()),
        ))]),
        must_not: None,
    }
}

/// Filter matching every point of one document (source + path).
fn doc_filter(source: &str, path: &str) -> Filter {
    Filter {
        should: None,
        min_should: None,
        must: Some(vec![
            Condition::Field(FieldCondition::new_match(
                "source".try_into().unwrap(),
                Match::from(source.to_string()),
            )),
            Condition::Field(FieldCondition::new_match(
                "path".try_into().unwrap(),
                Match::from(path.to_string()),
            )),
        ]),
        must_not: None,
    }
}

const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS meta(
     key TEXT PRIMARY KEY, value TEXT NOT NULL);
 CREATE TABLE IF NOT EXISTS documents(
     id INTEGER PRIMARY KEY,
     source TEXT NOT NULL,
     path TEXT NOT NULL,
     mtime INTEGER NOT NULL,
     content_hash TEXT NOT NULL,
     chunk_count INTEGER NOT NULL,
     updated_at INTEGER NOT NULL,
     UNIQUE(source, path));
 CREATE TABLE IF NOT EXISTS chunks(
     rowid INTEGER PRIMARY KEY,
     doc_id INTEGER NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
     ordinal INTEGER NOT NULL,
     content TEXT NOT NULL);";

impl Store {
    /// Open (or attach to) the store for `path`. Concurrent callers in the
    /// process share one instance — see [`REGISTRY`].
    pub async fn open(path: &Path) -> anyhow::Result<Store> {
        let registry = REGISTRY.get_or_init(|| Mutex::new(HashMap::new()));
        if let Some(shared) = registry.lock().unwrap().get(path).and_then(Weak::upgrade) {
            return Ok(Store { inner: shared });
        }
        // Held across creation: two first-opens would race on the
        // exclusive edge WAL and one would fail with WouldBlock.
        let _gate = OPEN_GATE.lock().await;
        // Re-check after waiting: another opener may have won the race.
        registry
            .lock()
            .unwrap()
            .retain(|_, w| w.upgrade().is_some());
        if let Some(shared) = registry.lock().unwrap().get(path).and_then(Weak::upgrade) {
            return Ok(Store { inner: shared });
        }
        let store = Store::open_exclusive(path).await?;
        registry
            .lock()
            .unwrap()
            .insert(path.to_path_buf(), Arc::downgrade(&store.inner));
        Ok(store)
    }

    async fn open_exclusive(path: &Path) -> anyhow::Result<Store> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建库目录失败 {}", parent.display()))?;
        }
        let opts = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true)
            // WAL allows a single writer; wait instead of failing fast
            // when another pool/txn briefly holds the write lock (mirrors
            // conga-host session_index).
            .busy_timeout(std::time::Duration::from_secs(5));
        // One connection per shared instance: the store is used by a
        // single sequential pipeline (ingest or search), so there is
        // nothing to gain from more and no intra-pool contention to
        // tolerate.
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .with_context(|| format!("打开库失败 {}", path.display()))?;
        sqlx::raw_sql(SCHEMA).execute(&pool).await?;
        // Reopen the vector shard when a previous ingest created it. A
        // leftover directory without a persisted config (crash between dir
        // creation and shard creation) is treated as absent and recreated
        // by the next `ensure_vec`.
        let dir = edge_dir(path);
        let shard = if dir.join(EDGE_CONFIG_FILE).exists() {
            Some(
                EdgeShard::load(&dir, None)
                    .with_context(|| format!("打开向量索引失败 {}", dir.display()))?,
            )
        } else {
            None
        };
        Ok(Store {
            inner: Arc::new(StoreShared {
                pool,
                shard: tokio::sync::Mutex::new(shard),
                db_path: path.to_path_buf(),
            }),
        })
    }

    /// Drop this handle; when it is the last one in the process, close
    /// the SQLite pool synchronously (checkpoint + remove `-wal`) and
    /// release the edge WAL lock. Concurrent handles keep the shared
    /// instance alive — their work continues unaffected.
    pub async fn close(self) -> anyhow::Result<()> {
        if let Ok(shared) = Arc::try_unwrap(self.inner) {
            shared.pool.close().await;
            // `shared` (and the EdgeShard with the WAL lock) drops here.
        }
        Ok(())
    }

    async fn meta_get(&self, key: &str) -> anyhow::Result<Option<String>> {
        Ok(
            sqlx::query_scalar::<_, String>("SELECT value FROM meta WHERE key = ?1")
                .bind(key)
                .fetch_optional(&self.inner.pool)
                .await?,
        )
    }

    async fn meta_set(&self, key: &str, value: &str) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO meta(key, value) VALUES(?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(key)
        .bind(value)
        .execute(&self.inner.pool)
        .await?;
        Ok(())
    }

    pub async fn fingerprint(&self) -> Option<(String, usize)> {
        let model = self.meta_get("embedding_model").await.ok()??;
        let dim: usize = self.meta_get("embedding_dim").await.ok()??.parse().ok()?;
        Some((model, dim))
    }

    /// Create the vector shard on first ingest; idempotent afterwards.
    /// Errors when the requested fingerprint differs from the stored one.
    pub async fn ensure_vec(&self, dim: usize, model: &str) -> anyhow::Result<()> {
        let stored = self.fingerprint().await;
        let dim = match &stored {
            Some((m, d)) => {
                anyhow::ensure!(
                    m == model && d == &dim,
                    "embedding 指纹变更:库中 {m}[{d}],请求 {model}[{dim}]。请使用 --rebuild 重建索引"
                );
                *d
            }
            None => dim,
        };
        // Slot lock, not a pre-check: two concurrent first-ingests must
        // not both create the shard.
        let mut slot = self.inner.shard.lock().await;
        if slot.is_some() {
            return Ok(()); // idempotent
        }
        let dir = edge_dir(&self.inner.db_path);
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("创建向量索引目录失败 {}", dir.display()))?;
        let config = EdgeConfig::builder()
            .on_disk_payload(false)
            .vector(
                qdrant_edge::DEFAULT_VECTOR_NAME,
                EdgeVectorParams::builder(dim, Distance::Cosine).build(),
            )
            .build();
        let shard = EdgeShard::new(&dir, config)
            .with_context(|| format!("创建向量索引失败 {}", dir.display()))?;
        if stored.is_none() {
            self.meta_set("embedding_model", model).await?;
            self.meta_set("embedding_dim", &dim.to_string()).await?;
        }
        *slot = Some(shard);
        Ok(())
    }

    /// Vector (point) count; 0 when no shard exists yet.
    pub async fn chunk_count(&self) -> anyhow::Result<i64> {
        let slot = self.inner.shard.lock().await;
        match &*slot {
            None => Ok(0),
            Some(shard) => Ok(shard.count(CountRequest::new())? as i64),
        }
    }

    pub async fn docs_for_source(&self, source: &str) -> anyhow::Result<Vec<DocRow>> {
        let rows: Vec<(String, i64, String)> =
            sqlx::query_as("SELECT path, mtime, content_hash FROM documents WHERE source = ?1")
                .bind(source)
                .fetch_all(&self.inner.pool)
                .await?;
        Ok(rows
            .into_iter()
            .map(|(path, mtime, content_hash)| DocRow {
                path: PathBuf::from(path),
                mtime,
                content_hash,
            })
            .collect())
    }

    pub async fn touch_mtime(&self, source: &str, path: &Path, mtime: i64) -> anyhow::Result<()> {
        sqlx::query("UPDATE documents SET mtime = ?1 WHERE source = ?2 AND path = ?3")
            .bind(mtime)
            .bind(source)
            .bind(path.to_string_lossy())
            .execute(&self.inner.pool)
            .await?;
        Ok(())
    }

    /// Document-level upsert in ONE SQLite transaction (old rows die via FK
    /// cascade, fresh chunks get new rowids), then the vector points are
    /// rewritten in the edge shard. The shard is derived data: points are
    /// deleted by (source, path) filter — which also sweeps stale points
    /// left by an earlier crashed run — and re-upserted with point ids
    /// matching the new chunk rowids. A crash between the SQLite commit and
    /// the edge write leaves the doc without vectors until its next content
    /// change (or `--rebuild`).
    pub async fn upsert_doc(
        &self,
        source: &str,
        path: &Path,
        mtime: i64,
        hash: &str,
        chunks: &[(usize, String, Vec<f32>)],
    ) -> anyhow::Result<()> {
        let mut tx = self.inner.pool.begin().await?;
        let p = path.to_string_lossy();
        sqlx::query("DELETE FROM documents WHERE source = ?1 AND path = ?2")
            .bind(source)
            .bind(&*p)
            .execute(&mut *tx)
            .await?;
        let doc_res = sqlx::query(
            "INSERT INTO documents(source, path, mtime, content_hash, chunk_count, updated_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(source)
        .bind(&*p)
        .bind(mtime)
        .bind(hash)
        .bind(chunks.len() as i64)
        .bind(chrono::Utc::now().timestamp())
        .execute(&mut *tx)
        .await?;
        let doc_id = doc_res.last_insert_rowid();
        let mut rowids = Vec::with_capacity(chunks.len());
        for (ordinal, content, _) in chunks {
            let chunk_res =
                sqlx::query("INSERT INTO chunks(doc_id, ordinal, content) VALUES(?1, ?2, ?3)")
                    .bind(doc_id)
                    .bind(*ordinal as i64)
                    .bind(content)
                    .execute(&mut *tx)
                    .await?;
            rowids.push(chunk_res.last_insert_rowid());
        }
        tx.commit().await?;

        if chunks.is_empty() {
            return Ok(());
        }
        let slot = self.inner.shard.lock().await;
        let Some(shard) = &*slot else {
            // The pipeline always calls ensure_vec before upserting.
            anyhow::bail!("向量索引未初始化:请先运行 conga-rag ingest");
        };
        shard.update(UpdateOperation::PointOperation(
            PointOperations::DeletePointsByFilter(doc_filter(source, &p)),
        ))?;
        let points = chunks
            .iter()
            .zip(rowids)
            .map(|((_, _, embedding), rowid)| {
                PointStruct::new(
                    rowid as u64,
                    embedding.clone(),
                    json!({"source": source, "path": &*p}),
                )
                .into()
            })
            .collect();
        shard.update(UpdateOperation::PointOperation(
            PointOperations::UpsertPoints(PointInsertOperations::PointsList(points)),
        ))?;
        Ok(())
    }

    /// Delete documents of `source` whose path is not in `live`. Returns
    /// count. Vector points die first (by filter, sweeping orphans), then
    /// the SQLite rows — same derived-data ordering contract as
    /// [`upsert_doc`].
    pub async fn remove_missing(&self, source: &str, live: &[PathBuf]) -> anyhow::Result<usize> {
        let rows: Vec<(i64, String)> =
            sqlx::query_as("SELECT id, path FROM documents WHERE source = ?1")
                .bind(source)
                .fetch_all(&self.inner.pool)
                .await?;
        let to_remove: Vec<String> = rows
            .iter()
            .filter(|(_, path)| !live.iter().any(|p| p.to_string_lossy() == *path))
            .map(|(_, path)| path.clone())
            .collect();
        let slot = self.inner.shard.lock().await;
        if let Some(shard) = &*slot {
            for path in &to_remove {
                shard.update(UpdateOperation::PointOperation(
                    PointOperations::DeletePointsByFilter(doc_filter(source, path)),
                ))?;
            }
        }
        let mut tx = self.inner.pool.begin().await?;
        for path in &to_remove {
            sqlx::query("DELETE FROM documents WHERE source = ?1 AND path = ?2")
                .bind(source)
                .bind(path)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(to_remove.len())
    }

    /// KNN over the edge shard. Errors when the index is empty (no shard).
    /// Hits join back to SQLite by point id (= chunk rowid), so orphaned
    /// points from a crashed run simply disappear from results.
    pub async fn knn(
        &self,
        query: &[f32],
        k: usize,
        source: Option<&str>,
    ) -> anyhow::Result<Vec<Hit>> {
        let slot = self.inner.shard.lock().await;
        let Some(shard) = &*slot else {
            anyhow::bail!("索引为空:请先运行 conga-rag ingest");
        };
        let mut builder = SearchRequestBuilder::new(
            QueryEnum::Nearest(NamedQuery {
                query: query.to_vec().into(),
                using: None,
            }),
            k,
        );
        if let Some(s) = source {
            builder = builder.filter(kw_filter("source", s));
        }
        let scored = shard.search(builder.build())?;
        if scored.is_empty() {
            return Ok(Vec::new());
        }
        // One round trip back into SQLite: ids travel as a bound JSON array
        // (never interpolated). Rows are re-ordered to the shard's
        // score-descending order below.
        let ids = serde_json::to_string(
            &scored
                .iter()
                .filter_map(|p| match p.id {
                    PointId::NumId(n) => Some(n as i64),
                    PointId::Uuid(_) => None,
                })
                .collect::<Vec<i64>>(),
        )?;
        let rows: Vec<(i64, String, String, i64, String)> = sqlx::query_as(
            "SELECT c.rowid, d.source, d.path, c.ordinal, c.content
             FROM chunks c JOIN documents d ON d.id = c.doc_id
             WHERE c.rowid IN (SELECT value FROM json_each(?1))",
        )
        .bind(ids)
        .fetch_all(&self.inner.pool)
        .await?;
        let by_id: HashMap<i64, (String, String, i64, String)> = rows
            .into_iter()
            .map(|(rowid, source, path, ordinal, content)| {
                (rowid, (source, path, ordinal, content))
            })
            .collect();
        Ok(scored
            .iter()
            .filter_map(|p| {
                let PointId::NumId(id) = p.id else {
                    return None;
                };
                by_id
                    .get(&(id as i64))
                    .map(|(source, path, ordinal, content)| Hit {
                        source: source.clone(),
                        path: path.clone(),
                        ordinal: *ordinal as usize,
                        content: content.clone(),
                        score: p.score as f64,
                    })
            })
            .collect())
    }

    pub async fn stats(&self) -> anyhow::Result<Vec<SourceStat>> {
        let rows: Vec<(String, i64, i64)> = sqlx::query_as(
            "SELECT d.source, count(DISTINCT d.id), count(c.rowid)
             FROM documents d LEFT JOIN chunks c ON c.doc_id = d.id
             GROUP BY d.source ORDER BY d.source",
        )
        .fetch_all(&self.inner.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(source, docs, chunks)| SourceStat {
                source,
                docs,
                chunks,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("t.db")).await.unwrap();
        (dir, s)
    }

    fn dim4(v: [f32; 4]) -> Vec<f32> {
        v.to_vec()
    }

    #[tokio::test]
    async fn open_creates_schema() {
        let (_d, s) = store().await;
        assert_eq!(s.chunk_count().await.unwrap(), 0);
        assert!(s.fingerprint().await.is_none());
        assert!(!edge_dir(&_d.path().join("t.db")).exists());
    }

    #[tokio::test]
    async fn ensure_vec_is_idempotent_and_locks_fingerprint() {
        let (_d, s) = store().await;
        s.ensure_vec(4, "m1").await.unwrap();
        s.ensure_vec(4, "m1").await.unwrap(); // idempotent
        assert_eq!(s.fingerprint().await.unwrap(), ("m1".to_string(), 4));
        let err = s.ensure_vec(8, "m2").await.unwrap_err();
        assert!(err.to_string().contains("--rebuild"), "err: {err}");
        // The persisted shard survives a reopen.
        drop(s);
        let s2 = Store::open(&_d.path().join("t.db")).await.unwrap();
        assert_eq!(s2.chunk_count().await.unwrap(), 0, "shard reopens empty");
    }

    #[tokio::test]
    async fn upsert_and_knn_roundtrip() {
        let (_d, s) = store().await;
        s.ensure_vec(4, "m1").await.unwrap();
        s.upsert_doc(
            "src",
            Path::new("/n/a.md"),
            1,
            "h1",
            &[
                (0, "alpha content".into(), dim4([1.0, 0.0, 0.0, 0.0])),
                (1, "beta content".into(), dim4([0.0, 1.0, 0.0, 0.0])),
            ],
        )
        .await
        .unwrap();
        s.upsert_doc(
            "src",
            Path::new("/n/b.md"),
            1,
            "h2",
            &[(0, "gamma content".into(), dim4([0.0, 0.0, 1.0, 0.0]))],
        )
        .await
        .unwrap();

        let hits = s.knn(&dim4([1.0, 0.0, 0.0, 0.0]), 2, None).await.unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].path, "/n/a.md");
        assert_eq!(hits[0].ordinal, 0);
        assert_eq!(hits[0].content, "alpha content");
        assert!(
            hits[0].score > 0.99,
            "identical vector → score≈1: {}",
            hits[0].score
        );
        assert!(hits[0].score >= hits[1].score);

        // source filter narrows results
        s.upsert_doc(
            "other",
            Path::new("/n/c.md"),
            1,
            "h3",
            &[(0, "delta content".into(), dim4([1.0, 0.0, 0.0, 0.0]))],
        )
        .await
        .unwrap();
        let only = s
            .knn(&dim4([1.0, 0.0, 0.0, 0.0]), 5, Some("other"))
            .await
            .unwrap();
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].source, "other");
    }

    #[tokio::test]
    async fn upsert_replaces_previous_chunks() {
        let (_d, s) = store().await;
        s.ensure_vec(4, "m1").await.unwrap();
        s.upsert_doc(
            "src",
            Path::new("/n/a.md"),
            1,
            "h1",
            &[(0, "old".into(), dim4([1.0, 0.0, 0.0, 0.0]))],
        )
        .await
        .unwrap();
        s.upsert_doc(
            "src",
            Path::new("/n/a.md"),
            2,
            "h1b",
            &[
                (0, "new".into(), dim4([0.0, 0.9, 0.0, 0.0])),
                (1, "new2".into(), dim4([0.0, 0.9, 0.1, 0.0])),
            ],
        )
        .await
        .unwrap();
        let hits = s.knn(&dim4([1.0, 0.0, 0.0, 0.0]), 10, None).await.unwrap();
        assert_eq!(hits.len(), 2, "old vector must be gone");
        assert!(hits.iter().all(|h| h.content.starts_with("new")));
        assert_eq!(s.chunk_count().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn remove_missing_deletes_only_absent() {
        let (_d, s) = store().await;
        s.ensure_vec(4, "m1").await.unwrap();
        s.upsert_doc(
            "src",
            Path::new("/n/a.md"),
            1,
            "h1",
            &[(0, "a".into(), dim4([1.0, 0.0, 0.0, 0.0]))],
        )
        .await
        .unwrap();
        s.upsert_doc(
            "src",
            Path::new("/n/gone.md"),
            1,
            "h2",
            &[(0, "g".into(), dim4([0.0, 1.0, 0.0, 0.0]))],
        )
        .await
        .unwrap();
        let removed = s
            .remove_missing("src", &[PathBuf::from("/n/a.md")])
            .await
            .unwrap();
        assert_eq!(removed, 1);
        assert_eq!(s.chunk_count().await.unwrap(), 1);
        let rows = s.docs_for_source("src").await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].path, Path::new("/n/a.md"));
    }

    #[tokio::test]
    async fn knn_without_vec_table_is_error() {
        let (_d, s) = store().await;
        let err = s.knn(&dim4([0.0; 4]), 3, None).await.unwrap_err();
        assert!(err.to_string().contains("ingest"), "err: {err}");
    }

    #[tokio::test]
    async fn orphaned_points_drop_out_of_knn() {
        // Simulate a crash between the SQLite commit and the edge write:
        // a point whose chunk row is gone must not surface in results.
        let (_d, s) = store().await;
        s.ensure_vec(4, "m1").await.unwrap();
        s.upsert_doc(
            "src",
            Path::new("/n/a.md"),
            1,
            "h1",
            &[(0, "live".into(), dim4([1.0, 0.0, 0.0, 0.0]))],
        )
        .await
        .unwrap();
        {
            let slot = s.inner.shard.lock().await;
            let shard = slot.as_ref().unwrap();
            shard
                .update(UpdateOperation::PointOperation(
                    PointOperations::UpsertPoints(PointInsertOperations::PointsList(vec![
                        PointStruct::new(
                            999_999u64,
                            dim4([0.9, 0.1, 0.0, 0.0]),
                            json!({"source": "src", "path": "/n/ghost"}),
                        )
                        .into(),
                    ])),
                ))
                .unwrap();
            // Guard must drop here: chunk_count/knn lock the same mutex.
        }
        assert_eq!(s.chunk_count().await.unwrap(), 2, "orphan is counted");
        let hits = s.knn(&dim4([1.0, 0.0, 0.0, 0.0]), 10, None).await.unwrap();
        assert_eq!(hits.len(), 1, "orphan never surfaces in knn");
        assert_eq!(hits[0].content, "live");
    }

    #[tokio::test]
    async fn stats_groups_by_source() {
        let (_d, s) = store().await;
        s.ensure_vec(4, "m1").await.unwrap();
        s.upsert_doc(
            "a",
            Path::new("/x/1"),
            1,
            "h",
            &[(0, "c".into(), dim4([1.0, 0.0, 0.0, 0.0]))],
        )
        .await
        .unwrap();
        s.upsert_doc(
            "b",
            Path::new("/x/2"),
            1,
            "h",
            &[
                (0, "c".into(), dim4([0.0, 1.0, 0.0, 0.0])),
                (1, "c2".into(), dim4([0.0, 0.0, 1.0, 0.0])),
            ],
        )
        .await
        .unwrap();
        let st = s.stats().await.unwrap();
        assert_eq!(st.len(), 2);
        let b = st.iter().find(|x| x.source == "b").unwrap();
        assert_eq!((b.docs, b.chunks), (1, 2));
    }

    #[tokio::test]
    async fn touch_mtime_updates_row() {
        let (_d, s) = store().await;
        s.ensure_vec(4, "m1").await.unwrap();
        s.upsert_doc(
            "s",
            Path::new("/n/a"),
            10,
            "h",
            &[(0, "c".into(), dim4([1.0, 0.0, 0.0, 0.0]))],
        )
        .await
        .unwrap();
        s.touch_mtime("s", Path::new("/n/a"), 99).await.unwrap();
        let rows = s.docs_for_source("s").await.unwrap();
        assert_eq!(rows[0].mtime, 99);
        assert_eq!(rows[0].content_hash, "h");
    }
}
