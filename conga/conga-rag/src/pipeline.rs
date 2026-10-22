//! Ingest pipeline: scan → clean → chunk → embed → store.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Context;
use sha2::{Digest, Sha256};

use crate::chunk::{chunk, Chunk};
use crate::clean::clean;
use crate::config::RagConfig;
use crate::embed::EmbeddingsClient;
use crate::source::DirSource;
use crate::store::{DocRow, Store};

#[derive(Debug, Default, Clone, PartialEq)]
pub struct IngestStats {
    pub scanned: usize,
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
    pub skipped: usize,
    pub failed: usize,
    pub chunks: usize,
}

/// A changed document waiting to be embedded and upserted.
struct Pending {
    source: String,
    path: PathBuf,
    mtime: i64,
    hash: String,
    chunks: Vec<Chunk>,
    /// Present in the store before this run (→ updated, not added).
    existed: bool,
}

/// Ingest every configured source (or just `only`): scan → remove deleted →
/// skip unchanged (mtime, then content hash) → clean → chunk → embed (one
/// flat batch for all pending docs of the run) → upsert.
pub async fn run_ingest(
    cfg: &RagConfig,
    only: Option<&str>,
    rebuild: bool,
) -> anyhow::Result<IngestStats> {
    let mut stats = IngestStats::default();
    let db_path = cfg.store_path();
    if rebuild && db_path.exists() {
        remove_store_files(&db_path)?;
    }
    let resolved = cfg.resolve_embedding()?;
    let client = EmbeddingsClient::new(&resolved);
    let store = Store::open(&db_path).await?;
    let mut pending: Vec<Pending> = Vec::new();

    for (name, src_cfg) in &cfg.sources {
        if let Some(o) = only {
            if o != name {
                continue;
            }
        }
        let dir = DirSource::new(name, src_cfg)?;
        let files = dir.scan()?;
        stats.scanned += files.len();
        tracing::info!(source = %name, files = files.len(), "扫描完成");
        let existing: HashMap<PathBuf, DocRow> = store
            .docs_for_source(name)
            .await?
            .into_iter()
            .map(|d| (d.path.clone(), d))
            .collect();

        // Removals first.
        stats.removed += store
            .remove_missing(
                name,
                &files.iter().map(|f| f.path.clone()).collect::<Vec<_>>(),
            )
            .await?;

        // Collect pending (changed) docs.
        for f in &files {
            // Nanosecond granularity: second-precision mtimes would falsely
            // skip a rewrite that lands within the same second.
            let mtime = fs::metadata(&f.path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(0);
            if let Some(prev) = existing.get(&f.path) {
                if prev.mtime == mtime {
                    stats.skipped += 1;
                    continue;
                }
            }
            let raw = match fs::read_to_string(&f.path) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("跳过不可读文件 {}: {e}", f.rel);
                    stats.failed += 1;
                    continue;
                }
            };
            let cleaned = clean(&raw);
            let hash = hex(&Sha256::digest(cleaned.as_bytes()));
            if let Some(prev) = existing.get(&f.path) {
                if prev.content_hash == hash {
                    store.touch_mtime(name, &f.path, mtime).await?;
                    stats.skipped += 1;
                    continue;
                }
            }
            let chunks = chunk(
                &cleaned,
                cfg.chunking.target_chars,
                cfg.chunking.overlap_chars,
            );
            let existed = existing.contains_key(&f.path);
            pending.push(Pending {
                source: name.clone(),
                path: f.path.clone(),
                mtime,
                hash,
                chunks,
                existed,
            });
        }
    }

    // Embed all pending chunks (across sources) in shared flat batches.
    if !pending.is_empty() {
        let flat: Vec<String> = pending
            .iter()
            .flat_map(|p| p.chunks.iter().map(|c| c.content.clone()))
            .collect();
        tracing::info!(
            docs = pending.len(),
            chunks = flat.len(),
            batch = resolved.batch,
            "开始嵌入"
        );
        let vectors = client
            .embed_batch(&flat)
            .await
            .context("embedding 调用失败")?;
        if !flat.is_empty() {
            let first_dim = vectors.first().map(|v| v.len()).unwrap_or(0);
            store.ensure_vec(first_dim, &resolved.model).await?;
        }
        let mut cursor = 0usize;
        for p in &pending {
            let n = p.chunks.len();
            let parts: Vec<(usize, String, Vec<f32>)> = p
                .chunks
                .iter()
                .enumerate()
                .zip(&vectors[cursor..cursor + n])
                .map(|((ordinal, c), v)| (ordinal, c.content.clone(), v.clone()))
                .collect();
            store
                .upsert_doc(&p.source, &p.path, p.mtime, &p.hash, &parts)
                .await?;
            stats.chunks += n;
            if p.existed {
                stats.updated += 1;
            } else {
                stats.added += 1;
            }
            cursor += n;
        }
    }
    store.close().await?;
    Ok(stats)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Remove the store file, its WAL sidecars (`<db>-wal`, `<db>-shm`) and
/// the qdrant-edge shard directory (`<db>.edge/`). Tolerates absence; an
/// orphaned `-wal` left by an unclean shutdown could otherwise replay onto
/// the freshly recreated db and resurrect the old index.
fn remove_store_files(db: &Path) -> anyhow::Result<()> {
    let sidecar = |suffix: &str| -> PathBuf {
        let mut s = db.as_os_str().to_os_string();
        s.push(suffix);
        PathBuf::from(s)
    };
    for p in [db.to_path_buf(), sidecar("-wal"), sidecar("-shm")] {
        match fs::remove_file(&p) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                return Err(anyhow::Error::new(e)
                    .context(format!("删除旧库失败(--rebuild): {}", p.display())));
            }
        }
    }
    let edge = crate::store::edge_dir(db);
    match fs::remove_dir_all(&edge) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(anyhow::Error::new(e)
                .context(format!("删除旧向量索引失败(--rebuild): {}", edge.display())));
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use crate::config::{RagConfig, SourceConfig};

    pub(crate) fn cfg_with_store(
        dir: &std::path::Path,
        db: &std::path::Path,
        base_url: &str,
    ) -> RagConfig {
        let mut sources = BTreeMap::new();
        sources.insert(
            "notes".to_string(),
            SourceConfig {
                kind: "dir".into(),
                path: dir.to_path_buf(),
                include: vec!["**/*.md".into()],
                exclude: vec![],
            },
        );
        RagConfig {
            sources,
            embedding: crate::config::EmbeddingConfig {
                base_url: Some(base_url.into()),
                api_key: Some("k".into()),
                model: Some("mock".into()),
                batch: 4,
                min_interval_ms: 0,
            },
            ..Default::default()
        }
        .with_store_path(db.to_path_buf())
    }

    #[tokio::test]
    async fn ingest_is_idempotent_and_removes_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let dbdir = tempfile::tempdir().unwrap();
        let (base, _req) = crate::testsupport::spawn_mock_embeddings(0).await;
        let cfg = cfg_with_store(dir.path(), &dbdir.path().join("t.db"), &base);

        std::fs::write(dir.path().join("match.md"), "# T\n\nmatch one").unwrap();
        std::fs::write(dir.path().join("other.md"), "other text").unwrap();

        let s1 = run_ingest(&cfg, None, false).await.unwrap();
        assert_eq!((s1.scanned, s1.added, s1.failed), (2, 2, 0));
        assert!(s1.chunks >= 2);

        let s2 = run_ingest(&cfg, None, false).await.unwrap();
        assert_eq!((s2.added, s2.updated, s2.removed, s2.chunks), (0, 0, 0, 0));
        assert_eq!(s2.skipped, 2);

        std::fs::remove_file(dir.path().join("other.md")).unwrap();
        std::fs::write(dir.path().join("match.md"), "# T\n\nmatch two").unwrap();
        let s3 = run_ingest(&cfg, None, false).await.unwrap();
        assert_eq!((s3.removed, s3.added, s3.updated), (1, 0, 1));
    }

    #[tokio::test]
    async fn rebuild_wipes_and_recreates() {
        let dir = tempfile::tempdir().unwrap();
        let dbdir = tempfile::tempdir().unwrap();
        let db = dbdir.path().join("t.db");
        let (base, _req) = crate::testsupport::spawn_mock_embeddings(0).await;
        let cfg = cfg_with_store(dir.path(), &db, &base);
        std::fs::write(dir.path().join("a.md"), "match").unwrap();
        run_ingest(&cfg, None, false).await.unwrap();
        let s = run_ingest(&cfg, None, true).await.unwrap();
        assert_eq!((s.added, s.scanned), (1, 1));
    }

    #[tokio::test]
    async fn rebuild_removes_wal_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        let dbdir = tempfile::tempdir().unwrap();
        let db = dbdir.path().join("t.db");
        let (base, _req) = crate::testsupport::spawn_mock_embeddings(0).await;
        let cfg = cfg_with_store(dir.path(), &db, &base);
        std::fs::write(dir.path().join("a.md"), "match").unwrap();
        run_ingest(&cfg, None, false).await.unwrap();

        // Simulate an unclean shutdown: orphaned WAL sidecars next to the db
        // and a stale marker inside the vector shard directory.
        std::fs::write(dbdir.path().join("t.db-wal"), b"stale wal").unwrap();
        std::fs::write(dbdir.path().join("t.db-shm"), b"stale shm").unwrap();
        let edge = crate::store::edge_dir(&db);
        std::fs::write(edge.join("stale.bin"), b"stale shard").unwrap();

        let s = run_ingest(&cfg, None, true).await.unwrap();
        assert_eq!(
            (s.added, s.scanned),
            (1, 1),
            "rebuild must start from a fresh index"
        );
        assert!(db.exists(), "fresh db file must be recreated");
        assert!(
            !dbdir.path().join("t.db-wal").exists(),
            "-wal sidecar must be gone"
        );
        assert!(
            !dbdir.path().join("t.db-shm").exists(),
            "-shm sidecar must be gone"
        );
        assert!(
            !edge.join("stale.bin").exists(),
            "stale edge shard contents must be gone"
        );

        // The doc is registered in the recreated index: a plain re-run skips it.
        let s2 = run_ingest(&cfg, None, false).await.unwrap();
        assert_eq!((s2.added, s2.skipped), (0, 1));
    }

    // --- helpers ---

    #[test]
    fn hex_encodes_bytes_in_lowercase() {
        assert_eq!(hex(&[]), "");
        assert_eq!(hex(&[0x00, 0x0f, 0xff]), "000fff");
        assert_eq!(hex(&[0xde, 0xad, 0xbe, 0xef]), "deadbeef");
    }

    #[test]
    fn hex_matches_the_sha256_digest() {
        let digest = sha2::Sha256::digest(b"abc");
        assert_eq!(
            hex(&digest),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn remove_store_files_removes_db_sidecars_and_the_edge_dir() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.db");
        std::fs::write(&db, b"db").unwrap();
        std::fs::write(dir.path().join("t.db-wal"), b"wal").unwrap();
        std::fs::write(dir.path().join("t.db-shm"), b"shm").unwrap();
        let edge = crate::store::edge_dir(&db);
        std::fs::create_dir_all(&edge).unwrap();
        std::fs::write(edge.join("point.bin"), b"shard").unwrap();

        remove_store_files(&db).unwrap();
        assert!(!db.exists(), "db file must be gone");
        assert!(!dir.path().join("t.db-wal").exists(), "-wal must be gone");
        assert!(!dir.path().join("t.db-shm").exists(), "-shm must be gone");
        assert!(!edge.exists(), "edge shard dir must be gone");
    }

    #[test]
    fn remove_store_files_tolerates_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("never-existed.db");
        remove_store_files(&db).unwrap();
        assert!(!db.exists());
    }

    #[test]
    fn remove_store_files_reports_a_failing_removal() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("t.db");
        // A directory cannot be unlinked: the removal must fail loudly.
        std::fs::create_dir_all(&db).unwrap();
        let err = remove_store_files(&db).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("--rebuild"), "msg: {msg}");
        assert!(msg.contains("t.db"), "msg must name the path: {msg}");
    }

    #[tokio::test]
    async fn ingest_fails_when_the_source_dir_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let dbdir = tempfile::tempdir().unwrap();
        let (base, _req) = crate::testsupport::spawn_mock_embeddings(0).await;
        let cfg = cfg_with_store(&dir.path().join("gone"), &dbdir.path().join("t.db"), &base);
        let err = run_ingest(&cfg, None, false).await.unwrap_err();
        assert!(
            err.to_string().contains("不存在"),
            "err must name the missing directory: {err:#}"
        );
    }

    /// `only` restricts the run to one source: the other source must stay
    /// untouched until it is ingested explicitly.
    #[tokio::test]
    async fn ingest_only_touches_the_selected_source() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let dbdir = tempfile::tempdir().unwrap();
        let (base, _req) = crate::testsupport::spawn_mock_embeddings(0).await;
        std::fs::write(a.path().join("a.md"), "match a").unwrap();
        std::fs::write(b.path().join("b.md"), "match b").unwrap();

        let mut cfg = cfg_with_store(a.path(), &dbdir.path().join("t.db"), &base);
        let other = crate::config::SourceConfig {
            kind: "dir".into(),
            path: b.path().to_path_buf(),
            include: vec!["**/*.md".into()],
            exclude: vec![],
        };
        cfg.sources.insert("other".to_string(), other);

        let s1 = run_ingest(&cfg, Some("notes"), false).await.unwrap();
        assert_eq!((s1.scanned, s1.added), (1, 1), "only the selected source");

        let s2 = run_ingest(&cfg, None, false).await.unwrap();
        assert_eq!((s2.scanned, s2.added), (2, 1), "the rest follows");
    }
}
