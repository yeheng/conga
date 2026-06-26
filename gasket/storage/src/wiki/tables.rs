use sqlx::SqlitePool;

/// Create wiki runtime/index tables.
///
/// Disk markdown files are the content SSOT (title/type/category/tags/summary/body).
/// `wiki_pages` is a derived projection holding runtime state (frequency,
/// access_count, last_accessed) and search-index staging. See `PageStore` for
/// the two-layer contract.
pub async fn create_wiki_tables(pool: &SqlitePool) -> anyhow::Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS wiki_pages (
            path         TEXT PRIMARY KEY,
            title        TEXT NOT NULL,
            type         TEXT NOT NULL,
            category     TEXT,
            tags         TEXT,
            content      TEXT NOT NULL DEFAULT '',
            summary      TEXT,
            created      TEXT NOT NULL,
            updated      TEXT NOT NULL,
            source_count INTEGER DEFAULT 0,
            confidence   REAL DEFAULT 1.0,
            checksum     TEXT,
            frequency    TEXT DEFAULT 'warm',
            access_count INTEGER DEFAULT 0,
            last_accessed TEXT,
            file_mtime   INTEGER,
            sync_sequence INTEGER DEFAULT 0
        )
        "#,
    )
    .execute(pool)
    .await?;

    // Indexes
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_wiki_pages_type ON wiki_pages(type)")
        .execute(pool)
        .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_wiki_pages_category ON wiki_pages(category)")
        .execute(pool)
        .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_wiki_pages_updated ON wiki_pages(updated)")
        .execute(pool)
        .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_wiki_pages_frequency ON wiki_pages(frequency)")
        .execute(pool)
        .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_wiki_pages_last_accessed ON wiki_pages(last_accessed)",
    )
    .execute(pool)
    .await?;

    Ok(())
}
