use anyhow::Result;
use sqlx::{Row, SqlitePool};

use super::types::Frequency;

/// A candidate for frequency decay.
#[derive(Debug, Clone)]
pub struct DecayCandidate {
    pub path: String,
    pub frequency: Frequency,
    pub last_accessed: String,
}

/// Input for upserting a wiki page into SQLite.
#[derive(Debug)]
pub struct WikiPageInput<'a> {
    pub path: &'a str,
    pub title: &'a str,
    pub page_type: &'a str,
    pub category: Option<&'a str>,
    pub tags: &'a str,
    pub summary: Option<&'a str>,
    pub content: &'a str,
    pub source_count: u32,
    pub confidence: f64,
    pub checksum: Option<&'a str>,
    pub frequency: Frequency,
    pub access_count: u64,
    pub last_accessed: Option<String>,
    pub file_mtime: i64,
}

/// SQLite projection of wiki pages — derived runtime/index state.
///
/// Disk markdown files are the content SSOT; this table mirrors content for
/// query/index staging and holds runtime-only fields (frequency, access_count,
/// last_accessed). See `PageStore` for the two-layer contract.
#[derive(Clone)]
pub struct WikiPageStore {
    pool: SqlitePool,
}

impl WikiPageStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Atomic UPSERT. SQLite WAL handles concurrency.
    pub async fn upsert(&self, page: &WikiPageInput<'_>) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query(
            r#"
            INSERT INTO wiki_pages (path, title, type, category, tags, summary, content, created, updated, source_count, confidence, checksum, frequency, access_count, last_accessed, file_mtime)
            VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16)
            ON CONFLICT(path) DO UPDATE SET
                title = excluded.title,
                type = excluded.type,
                category = excluded.category,
                tags = excluded.tags,
                summary = excluded.summary,
                content = excluded.content,
                updated = excluded.updated,
                source_count = excluded.source_count,
                confidence = excluded.confidence,
                checksum = excluded.checksum,
                frequency = excluded.frequency,
                access_count = excluded.access_count,
                last_accessed = excluded.last_accessed,
                file_mtime = excluded.file_mtime
            "#,
        )
        .bind(page.path)
        .bind(page.title)
        .bind(page.page_type)
        .bind(page.category)
        .bind(page.tags)
        .bind(page.summary)
        .bind(page.content)
        .bind(&now)
        .bind(&now)
        .bind(page.source_count as i64)
        .bind(page.confidence)
        .bind(page.checksum)
        .bind(page.frequency.to_string())
        .bind(page.access_count as i64)
        .bind(page.last_accessed.as_deref())
        .bind(page.file_mtime)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get(&self, path: &str) -> Result<Option<PageRow>> {
        let row = sqlx::query_as::<_, PageRow>(
            "SELECT path, title, type, category, tags, summary, content, created, updated, source_count, confidence, checksum, frequency, access_count, last_accessed, file_mtime FROM wiki_pages WHERE path = $1"
        )
        .bind(path)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    pub async fn delete(&self, path: &str) -> Result<()> {
        sqlx::query("DELETE FROM wiki_pages WHERE path = $1")
            .bind(path)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn exists(&self, path: &str) -> Result<bool> {
        let row: Option<(String,)> = sqlx::query_as("SELECT path FROM wiki_pages WHERE path = $1")
            .bind(path)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.is_some())
    }

    pub async fn list_by_type(&self, page_type: &str) -> Result<Vec<PageRow>> {
        let rows = sqlx::query_as::<_, PageRow>(
            "SELECT path, title, type, category, tags, summary, content, created, updated, source_count, confidence, checksum, frequency, access_count, last_accessed, file_mtime FROM wiki_pages WHERE type = $1 ORDER BY updated DESC"
        )
        .bind(page_type)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn list_all(&self) -> Result<Vec<PageRow>> {
        let rows = sqlx::query_as::<_, PageRow>(
            "SELECT path, title, type, category, tags, summary, content, created, updated, source_count, confidence, checksum, frequency, access_count, last_accessed, file_mtime FROM wiki_pages ORDER BY updated DESC"
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// Get decay candidates: pages whose last_accessed is older than `days` and
    /// are not already archived.
    pub async fn get_decay_candidates(&self, days: i64) -> Result<Vec<DecayCandidate>> {
        let cutoff = chrono::Utc::now() - chrono::Duration::days(days);
        let cutoff_str = cutoff.to_rfc3339();
        let rows = sqlx::query(
            "SELECT path, frequency, last_accessed
             FROM wiki_pages
             WHERE frequency != 'archived'
               AND last_accessed IS NOT NULL
               AND last_accessed != ''
               AND datetime(last_accessed) < ?",
        )
        .bind(&cutoff_str)
        .fetch_all(&self.pool)
        .await?;
        let candidates: Vec<_> = rows
            .into_iter()
            .map(|row| {
                let freq_str: String = row.get("frequency");
                DecayCandidate {
                    path: row.get("path"),
                    frequency: Frequency::from_str_lossy(&freq_str),
                    last_accessed: row.get("last_accessed"),
                }
            })
            .collect();
        Ok(candidates)
    }

    /// Update only the frequency of a page (used by decay).
    pub async fn update_frequency(&self, path: &str, frequency: Frequency) -> Result<bool> {
        let result = sqlx::query("UPDATE wiki_pages SET frequency = ? WHERE path = ?")
            .bind(frequency.to_string())
            .bind(path)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    /// Batch-load full pages for a set of paths.
    ///
    /// Uses `SELECT * ... WHERE path IN (...)` queries chunked to stay within
    /// SQLite's binding variable limit (999 on older versions, 32766 on newer).
    pub async fn get_many(&self, paths: &[String]) -> Result<Vec<PageRow>> {
        if paths.is_empty() {
            return Ok(vec![]);
        }

        const CHUNK_SIZE: usize = 900;
        let mut results = Vec::new();

        for chunk in paths.chunks(CHUNK_SIZE) {
            let placeholders: String = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "SELECT path, title, type, category, tags, summary, content, created, updated, source_count, confidence, checksum, frequency, access_count, last_accessed, file_mtime \
                 FROM wiki_pages \
                 WHERE path IN ({})",
                placeholders
            );

            let mut query = sqlx::query_as::<_, PageRow>(&sql);
            for p in chunk {
                query = query.bind(p);
            }

            let rows = query.fetch_all(&self.pool).await?;
            results.extend(rows);
        }

        Ok(results)
    }

    /// Batch-load lightweight page summaries for a set of paths.
    ///
    /// Returns only the metadata fields needed for reranking and display
    /// (title, tags, confidence, etc.) — NOT the heavy `content` column.
    /// This is the N+1 fix: one query for N paths instead of N separate
    /// `SELECT *` queries pulling megabytes of content into memory.
    ///
    /// Queries are chunked to stay within SQLite's binding variable limit.
    pub async fn get_summaries_by_paths(&self, paths: &[String]) -> Result<Vec<PageSummaryRow>> {
        if paths.is_empty() {
            return Ok(vec![]);
        }

        const CHUNK_SIZE: usize = 900;
        let mut results = Vec::new();

        for chunk in paths.chunks(CHUNK_SIZE) {
            let placeholders: String = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "SELECT path, title, type, category, tags, summary, confidence, frequency, access_count, last_accessed, updated, LENGTH(content) as content_length, file_mtime \
                 FROM wiki_pages \
                 WHERE path IN ({})",
                placeholders
            );

            let mut query = sqlx::query_as::<_, PageSummaryRow>(&sql);
            for p in chunk {
                query = query.bind(p);
            }

            let rows = query.fetch_all(&self.pool).await?;
            results.extend(rows);
        }

        Ok(results)
    }
}

/// Lightweight page summary row — raw DB representation.
#[derive(Debug, sqlx::FromRow)]
pub struct PageSummaryRow {
    pub path: String,
    pub title: String,
    #[sqlx(rename = "type")]
    pub page_type: String,
    pub category: Option<String>,
    pub tags: Option<String>,
    pub summary: Option<String>,
    pub confidence: f64,
    pub frequency: String,
    pub access_count: i64,
    pub last_accessed: Option<String>,
    pub updated: String,
    /// Content length in bytes (for budget-aware selection without loading full content).
    #[sqlx(rename = "content_length")]
    pub content_length: i64,
    pub file_mtime: i64,
}

#[derive(Debug, sqlx::FromRow)]
pub struct PageRow {
    pub path: String,
    pub title: String,
    #[sqlx(rename = "type")]
    pub page_type: String,
    pub category: Option<String>,
    pub tags: Option<String>,
    pub summary: Option<String>,
    pub content: String,
    pub created: String,
    pub updated: String,
    pub source_count: i64,
    pub confidence: f64,
    pub checksum: Option<String>,
    pub frequency: String,
    pub access_count: i64,
    pub last_accessed: Option<String>,
    pub file_mtime: i64,
}
