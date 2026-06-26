//! Wiki knowledge system CLI commands.

use anyhow::Result;
use gasket_engine::wiki::{slugify, PageFilter, PageStore, PageType, WikiLinter, WikiPage};
use std::path::PathBuf;
use std::sync::Arc;

fn wiki_base_dir() -> PathBuf {
    dirs::home_dir()
        .map(|p| p.join(".gasket/wiki"))
        .expect("Could not find home directory")
}

/// Initialize wiki directory structure and SQLite tables.
pub async fn cmd_wiki_init() -> Result<()> {
    let wiki_root = wiki_base_dir();
    let store = gasket_engine::SqliteStore::new().await?;
    let ps = PageStore::new(store.pool(), wiki_root.clone());
    ps.init_dirs().await?;

    // Create wiki tables
    gasket_engine::create_wiki_tables(&store.pool()).await?;

    println!("Wiki initialized at {}", wiki_root.display());
    Ok(())
}

/// Migrate existing memory files (~/.gasket/memory/) to wiki pages.
pub async fn cmd_wiki_migrate() -> Result<()> {
    let memory_dir = dirs::home_dir()
        .map(|p| p.join(".gasket/memory"))
        .expect("Could not find home directory");

    if !memory_dir.exists() {
        println!(
            "No memory directory found at {}. Nothing to migrate.",
            memory_dir.display()
        );
        return Ok(());
    }

    let wiki_root = wiki_base_dir();
    let store = gasket_engine::SqliteStore::new().await?;

    // Ensure wiki tables exist
    gasket_engine::create_wiki_tables(&store.pool()).await?;

    let ps = PageStore::new(store.pool(), wiki_root.clone());
    ps.init_dirs().await?;

    let mut migrated = 0;
    let mut errors = 0;

    let scenario_map = [
        ("profile", "entities/people"),
        ("active", "topics"),
        ("knowledge", "topics"),
        ("decisions", "topics"),
        ("episodes", "topics"),
        ("reference", "sources"),
    ];

    for (scenario, prefix) in &scenario_map {
        let dir = memory_dir.join(scenario);
        if !dir.exists() {
            continue;
        }

        let mut entries = std::fs::read_dir(&dir)?;
        while let Some(entry) = entries.next().transpose()? {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "md") {
                continue;
            }

            let content = match std::fs::read_to_string(&path) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("Failed to read {}: {}", path.display(), e);
                    errors += 1;
                    continue;
                }
            };

            let title = extract_title(&content).unwrap_or_else(|| {
                path.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("untitled")
                    .to_string()
            });

            let slug = slugify(&title);
            let page_path = format!("{}/{}", prefix, slug);

            let body = skip_frontmatter(&content);

            let mut page = WikiPage::new(page_path, title, PageType::Topic, body.to_string());

            if let Some(tags) = extract_tags(&content) {
                page.tags = tags;
            }

            match ps.write(&page).await {
                Ok(_) => migrated += 1,
                Err(e) => {
                    tracing::warn!("Failed to write page {}: {}", page.path, e);
                    errors += 1;
                }
            }
        }
    }

    println!("Migration complete:");
    println!("  {} pages migrated", migrated);
    if errors > 0 {
        println!("  {} errors", errors);
    }

    let cached = ps.sync_db_from_disk().await?;
    println!("  {} disk cache files synced to index", cached);

    // Rebuild Tantivy index from SQLite
    let tantivy_dir = wiki_root.join(".tantivy");
    std::fs::create_dir_all(&tantivy_dir).ok();
    match gasket_storage::wiki::TantivyPageIndex::open(tantivy_dir) {
        Ok(tantivy_idx) => {
            let index = gasket_engine::wiki::PageIndex::new(Arc::new(tantivy_idx));
            let count = index.rebuild(&ps).await?;
            println!("  Tantivy index rebuilt with {} pages", count);
        }
        Err(e) => {
            println!("  Tantivy index rebuild skipped: {}", e);
        }
    }

    // If no errors, remove empty memory directories
    if errors == 0 {
        let mut all_empty = true;
        for (scenario, _) in &scenario_map {
            let dir = memory_dir.join(scenario);
            if dir.exists() {
                let has_files = std::fs::read_dir(&dir)?
                    .any(|e| e.ok().map(|entry| entry.path().is_file()).unwrap_or(false));
                if has_files {
                    all_empty = false;
                } else {
                    std::fs::remove_dir(&dir).ok();
                }
            }
        }
        if all_empty {
            std::fs::remove_dir(&memory_dir).ok();
            println!("  Old memory directory removed");
        }
    }

    Ok(())
}

/// Show wiki statistics.
pub async fn cmd_wiki_stats() -> Result<()> {
    let wiki_root = wiki_base_dir();
    if !wiki_root.exists() {
        println!("Wiki not initialized. Run 'gasket wiki init' first.");
        return Ok(());
    }

    let store = gasket_engine::SqliteStore::new().await?;
    let ps = PageStore::new(store.pool(), wiki_root);

    let all = ps.list(PageFilter::default()).await?;
    let entities = all
        .iter()
        .filter(|p| p.page_type == PageType::Entity)
        .count();
    let topics = all
        .iter()
        .filter(|p| p.page_type == PageType::Topic)
        .count();
    let sources = all
        .iter()
        .filter(|p| p.page_type == PageType::Source)
        .count();

    println!("Wiki Statistics:");
    println!("  Total pages: {}", all.len());
    println!("  Entities: {}", entities);
    println!("  Topics: {}", topics);
    println!("  Sources: {}", sources);

    Ok(())
}

/// Ingest a file into the wiki.
pub async fn cmd_wiki_ingest(path: &str, tier: &str) -> Result<()> {
    let file_path = PathBuf::from(path);
    if !file_path.exists() {
        anyhow::bail!("File not found: {}", path);
    }

    let wiki_root = wiki_base_dir();
    let store = gasket_engine::SqliteStore::new().await?;
    gasket_engine::create_wiki_tables(&store.pool()).await?;

    let ps = PageStore::new(store.pool().clone(), wiki_root);
    ps.init_dirs().await?;

    let content = std::fs::read_to_string(&file_path)?;
    let title = file_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("untitled")
        .to_string();

    let slug = slugify(&title);
    let page_path = format!("topics/{}", slug);

    let mut page = WikiPage::new(page_path.clone(), title.clone(), PageType::Topic, content);

    if path.contains("entity") || path.contains("entities") {
        page.page_type = PageType::Entity;
        page.path = format!("entities/{}", slug);
    } else if path.contains("source") || path.contains("sources") {
        page.page_type = PageType::Source;
        page.path = format!("sources/{}", slug);
    }

    if tier == "deep" {
        println!("Deep ingest requires LLM provider. Falling back to quick ingest.");
    }

    ps.write(&page).await?;
    println!("Ingested '{}' → '{}'", path, page.path);
    Ok(())
}

/// Search wiki pages.
pub async fn cmd_wiki_search(query: &str, limit: usize) -> Result<()> {
    let wiki_root = wiki_base_dir();
    if !wiki_root.exists() {
        println!("Wiki not initialized. Run 'gasket wiki init' first.");
        return Ok(());
    }

    let store = gasket_engine::SqliteStore::new().await?;
    let ps = PageStore::new(store.pool(), wiki_root);

    let tantivy_dir = dirs::home_dir()
        .map(|p| p.join(".gasket/wiki/.tantivy"))
        .unwrap();

    if tantivy_dir.exists() {
        match gasket_storage::wiki::TantivyPageIndex::open(tantivy_dir) {
            Ok(tantivy_idx) => {
                let index = gasket_engine::wiki::PageIndex::new(Arc::new(tantivy_idx));
                let hits = index.search_raw(query, limit).await?;
                if hits.is_empty() {
                    println!("No results for '{}'", query);
                    return Ok(());
                }
                println!("Search results for '{}' ({} hits):\n", query, hits.len());
                for (i, hit) in hits.iter().enumerate() {
                    println!(
                        "  {}. {} ({}) — score: {:.2}",
                        i + 1,
                        hit.title,
                        hit.path,
                        hit.score
                    );
                }
                return Ok(());
            }
            Err(e) => {
                tracing::debug!("Tantivy search failed, falling back to list: {}", e);
            }
        }
    }

    // Fallback: list all pages and do simple matching
    let pages = ps.list(PageFilter::default()).await?;
    let query_lower = query.to_lowercase();
    let matches: Vec<_> = pages
        .iter()
        .filter(|p| {
            p.title.to_lowercase().contains(&query_lower)
                || p.path.to_lowercase().contains(&query_lower)
        })
        .take(limit)
        .collect();

    if matches.is_empty() {
        println!("No results for '{}'", query);
    } else {
        println!("Search results for '{}' ({} hits):\n", query, matches.len());
        for (i, page) in matches.iter().enumerate() {
            println!("  {}. {} ({})", i + 1, page.title, page.path);
        }
    }
    Ok(())
}

/// List wiki pages.
pub async fn cmd_wiki_list(page_type: Option<&str>) -> Result<()> {
    let wiki_root = wiki_base_dir();
    if !wiki_root.exists() {
        println!("Wiki not initialized. Run 'gasket wiki init' first.");
        return Ok(());
    }

    let store = gasket_engine::SqliteStore::new().await?;
    let ps = PageStore::new(store.pool(), wiki_root);

    let filter = match page_type {
        Some("entity") => PageFilter {
            page_type: Some(PageType::Entity),
            ..Default::default()
        },
        Some("topic") => PageFilter {
            page_type: Some(PageType::Topic),
            ..Default::default()
        },
        Some("source") => PageFilter {
            page_type: Some(PageType::Source),
            ..Default::default()
        },
        _ => PageFilter::default(),
    };

    let pages = ps.list(filter).await?;
    if pages.is_empty() {
        println!("No wiki pages found.");
        return Ok(());
    }

    println!("Wiki pages ({} total):\n", pages.len());
    for page in &pages {
        let type_tag = match page.page_type {
            PageType::Entity => "[E]",
            PageType::Topic => "[T]",
            PageType::Source => "[S]",
            PageType::Sop => "[P]",
        };
        let tags = if page.tags.is_empty() {
            String::new()
        } else {
            format!(" [{}]", page.tags.join(", "))
        };
        println!("  {} {} ({}){}", type_tag, page.title, page.path, tags);
    }
    Ok(())
}

/// Delete a wiki page.
pub async fn cmd_wiki_delete(path: &str, force: bool) -> Result<()> {
    let wiki_root = wiki_base_dir();
    if !wiki_root.exists() {
        println!("Wiki not initialized. Run 'gasket wiki init' first.");
        return Ok(());
    }

    if !force {
        print!("Are you sure you want to delete '{}'? [y/N] ", path);
        use std::io::Write;
        std::io::stdout().flush()?;
        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Cancelled.");
            return Ok(());
        }
    }

    let store = gasket_engine::SqliteStore::new().await?;
    let ps = PageStore::new(store.pool(), wiki_root.clone());

    match ps.delete(path).await {
        Ok(_) => {
            println!("Deleted wiki page: {}", path);
        }
        Err(e) => {
            println!("Failed to delete '{}': {}", path, e);
            return Ok(());
        }
    }

    // CLI has no background WikiIndexingService, so do synchronous Tantivy cleanup here.
    let tantivy_dir = wiki_root.join(".tantivy");
    if tantivy_dir.exists() {
        match gasket_storage::wiki::TantivyPageIndex::open(tantivy_dir) {
            Ok(tantivy_idx) => {
                let index = gasket_engine::wiki::PageIndex::new(Arc::new(tantivy_idx));
                if let Err(e) = index.delete(path).await {
                    tracing::warn!("Failed to remove page from Tantivy index: {}", e);
                }
            }
            Err(e) => {
                tracing::debug!("Tantivy index not available for cleanup: {}", e);
            }
        }
    }

    Ok(())
}

/// Run wiki lint checks.
pub async fn cmd_wiki_lint(auto_fix: bool) -> Result<()> {
    let wiki_root = wiki_base_dir();
    if !wiki_root.exists() {
        println!("Wiki not initialized. Run 'gasket wiki init' first.");
        return Ok(());
    }

    let store = gasket_engine::SqliteStore::new().await?;
    let ps = PageStore::new(store.pool(), wiki_root);

    let linter = WikiLinter::new(ps);

    println!("Running wiki lint...");
    let report = linter.lint().await?;

    println!("{}", report.summary());

    if auto_fix && report.total_issues() > 0 {
        let fix_report = linter.auto_fix(&report).await?;
        if fix_report.total_fixes() > 0 {
            println!("\nAuto-fixed {} issues.", fix_report.total_fixes());
        }
    }

    Ok(())
}

// ── Helpers ──────────────────────────────────────────────────

fn extract_title(content: &str) -> Option<String> {
    if !content.trim_start().starts_with("---") {
        return None;
    }
    let rest = &content.trim_start()[3..];
    let end = rest.find("\n---")?;
    let yaml = &rest[..end];
    for line in yaml.lines() {
        if line.trim().starts_with("title:") {
            return Some(line.trim().trim_start_matches("title:").trim().to_string());
        }
    }
    None
}

fn extract_tags(content: &str) -> Option<Vec<String>> {
    if !content.trim_start().starts_with("---") {
        return None;
    }
    let rest = &content.trim_start()[3..];
    let end = rest.find("\n---")?;
    let yaml = &rest[..end];
    for line in yaml.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("tags:") {
            let tags_str = trimmed.trim_start_matches("tags:").trim();
            if tags_str.is_empty() || tags_str == "[]" {
                let mut tags = Vec::new();
                let yaml_lines: Vec<&str> = yaml.lines().collect();
                if let Some(idx) = yaml_lines
                    .iter()
                    .position(|l| l.trim().starts_with("tags:"))
                {
                    for following in yaml_lines[idx + 1..].iter() {
                        let f = following.trim();
                        if f.starts_with("- ") {
                            tags.push(f.trim_start_matches("- ").trim().to_string());
                        } else if !f.starts_with("- ") && !f.is_empty() {
                            break;
                        }
                    }
                }
                if !tags.is_empty() {
                    return Some(tags);
                }
            } else {
                let tags: Vec<String> = tags_str
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                if !tags.is_empty() {
                    return Some(tags);
                }
            }
        }
    }
    None
}

fn skip_frontmatter(content: &str) -> &str {
    let content = content.trim_start();
    if content.starts_with("---") {
        if let Some(end) = content[3..].find("\n---") {
            return content[end + 7..].trim();
        }
    }
    content.trim()
}
