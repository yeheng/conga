//! Memory: a prompt-appended catalog of distilled experience notes.
//!
//! Entries live at `<config_dir>/memory/*.md` — frontmatter
//! (`title`/`tags`/`created`/`source_session`) plus a short body. Only the
//! catalog line rides in the system prompt; the model pulls the full entry
//! with the `read` tool on demand (progressive disclosure, same contract as
//! skills). The catalog is deterministic (sorted by title) so the prompt
//! stays byte-stable across turns — same files, same bytes, warm provider
//! cache prefix.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Hard cap on catalog lines (and on admission in `evolve`). The library is
/// deliberately small: it must stay cheap to scan every turn and readable
/// by a human auditing it.
pub const MAX_ENTRIES: usize = 64;

/// Catalog preview is bounded to one line (the first body line): the catalog
/// rides in the system prompt, paid on every request.
const MAX_PREVIEW_CHARS: usize = 120;

pub struct MemoryEntry {
    pub title: String,
    pub tags: Vec<String>,
    pub source: PathBuf,
    /// First non-empty body line, bounded — the catalog hook.
    pub preview: String,
}

/// Production entry: root is conga's config dir (`~/.conga`).
pub fn append_memory(base: &str) -> String {
    append_memory_in(base, &conga::storage::config_dir().join("memory"))
}

/// Testable core: `root` is injected (production uses the config dir).
pub fn append_memory_in(base: &str, root: &Path) -> String {
    let entries = catalog_entries(root);
    if entries.is_empty() {
        return base.to_string();
    }
    let mut out = String::from(base);
    out.push_str(
        "\n\n## Memory\n\nThe entries below are lessons distilled from past \
         sessions (title [tags] — summary). When one is relevant to the current \
         task, load its full content with the `read` tool using the path shown.\n\n",
    );
    for (title, e) in &entries {
        out.push_str(&format!(
            "- {} [{}] — {} (source: {})\n",
            title,
            e.tags.join(", "),
            e.preview,
            e.source.display()
        ));
    }
    out
}

/// Sorted view used by both the catalog and evolve's dedupe check.
/// `load_entries` reports disk truth (no cap; duplicate titles resolve to
/// the lexicographically smallest source path so iteration order never
/// decides) — the cap is a catalog and admission concern, not a storage one.
pub fn load_entries(root: &Path) -> Vec<MemoryEntry> {
    let mut map: BTreeMap<String, MemoryEntry> = BTreeMap::new();
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return Vec::new(), // no dir = no entries; not an error
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        match std::fs::read_to_string(&path)
            .ok()
            .and_then(|c| parse_entry(&c))
        {
            Some((title, tags, preview)) => {
                let entry = MemoryEntry {
                    title: title.clone(),
                    tags,
                    source: path,
                    preview,
                };
                match map.entry(title) {
                    std::collections::btree_map::Entry::Occupied(mut occupied) => {
                        // Two files may carry the same `title:`; keep the
                        // lexicographically smaller source path so the
                        // catalog never depends on read_dir order
                        // (byte-stable prompt invariant).
                        if entry.source < occupied.get().source {
                            occupied.insert(entry);
                        }
                    }
                    std::collections::btree_map::Entry::Vacant(vacant) => {
                        vacant.insert(entry);
                    }
                }
            }
            None => tracing::warn!(
                path = %path.display(),
                "memory entry missing frontmatter title/tags; skipped"
            ),
        }
    }
    map.into_values().collect()
}

fn catalog_entries(root: &Path) -> BTreeMap<String, MemoryEntry> {
    load_entries(root)
        .into_iter()
        .take(MAX_ENTRIES)
        .map(|e| (e.title.clone(), e))
        .collect()
}

/// Canonical file body for a new entry (evolve writes with this).
pub fn entry_markdown(
    title: &str,
    tags: &[String],
    created: &str,
    source_session: &str,
    body: &str,
) -> String {
    format!(
        "---\ntitle: {title}\ntags: [{}]\ncreated: {created}\nsource_session: {source_session}\n---\n{}\n",
        tags.join(", "),
        body.trim()
    )
}

/// Hand-parsed frontmatter: `---\n` … `---` block carrying `title:` and
/// `tags:` (`[a, b]` list). `created`/`source_session` are provenance and
/// not re-parsed. The preview is the first non-empty body line after the
/// block, bounded to [`MAX_PREVIEW_CHARS`] on a char boundary.
fn parse_entry(content: &str) -> Option<(String, Vec<String>, String)> {
    let rest = content.strip_prefix("---\n")?;
    let end = rest.find("\n---")?;
    let fm = &rest[..end];
    let body = &rest[end + 4..];
    let mut title = None;
    let mut tags = None;
    for line in fm.lines() {
        if let Some(v) = line.strip_prefix("title:") {
            title = Some(unquote(v.trim()));
        } else if let Some(v) = line.strip_prefix("tags:") {
            let inner = v.trim().trim_start_matches('[').trim_end_matches(']');
            tags = Some(
                inner
                    .split(',')
                    .map(|t| unquote(t.trim()))
                    .filter(|t| !t.is_empty())
                    .collect::<Vec<_>>(),
            );
        }
    }
    let title = title.filter(|t| !t.is_empty())?;
    let tags = tags.unwrap_or_default();
    let preview = body
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| bound_chars(l, MAX_PREVIEW_CHARS))
        .unwrap_or_default();
    Some((title, tags, preview))
}

/// Char-boundary-safe truncation with an ellipsis (a plain
/// `String::truncate` panics mid-codepoint).
fn bound_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max).collect();
    t.push('…');
    t
}

/// Strip one balanced pair of surrounding quotes (same rule as skills).
fn unquote(v: &str) -> String {
    let b = v.as_bytes();
    if b.len() >= 2
        && ((b[0] == b'"' && b[b.len() - 1] == b'"') || (b[0] == b'\'' && b[b.len() - 1] == b'\''))
    {
        v[1..v.len() - 1].to_string()
    } else {
        v.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &std::path::Path, name: &str, body: &str) {
        std::fs::write(root.join(name), body).unwrap();
    }

    #[test]
    fn parses_title_tags_and_builds_catalog() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("memory");
        std::fs::create_dir_all(&root).unwrap();
        write(
            &root,
            "a.md",
            "---\ntitle: rust-cyclic-dep\ntags: [rust, cargo, build-error]\ncreated: 1770000000\nsource_session: s1\n---\nCheck workspace members' path refs first, then [patch].\nsecond line ignored\n",
        );
        let out = append_memory_in("BASE", &root);
        assert!(out.starts_with("BASE"));
        assert!(out.contains("## Memory"));
        assert!(out.contains("rust-cyclic-dep"));
        assert!(out.contains("[rust, cargo, build-error]"));
        assert!(out.contains("Check workspace members' path refs first, then [patch]."));
        assert!(!out.contains("second line ignored"));
        assert!(out.contains(&root.join("a.md").display().to_string()));
    }

    #[test]
    fn missing_or_empty_dir_is_noop() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(append_memory_in("BASE", &tmp.path().join("memory")), "BASE");
    }

    #[test]
    fn unparsable_entry_skipped_with_warn() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("memory");
        std::fs::create_dir_all(&root).unwrap();
        write(&root, "bad.md", "no frontmatter at all");
        write(
            &root,
            "good.md",
            "---\ntitle: good\ntags: [x]\ncreated: 1\nsource_session: s\n---\nBody line.\n",
        );
        let entries = load_entries(&root);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].title, "good");
        let out = append_memory_in("BASE", &root);
        assert!(out.contains("good"));
    }

    #[test]
    fn catalog_sorted_and_capped_at_64() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("memory");
        std::fs::create_dir_all(&root).unwrap();
        for i in 0..70 {
            write(&root, format!("{i:03}.md").as_str(),
                &format!("---\ntitle: t{i:03}\ntags: [t]\ncreated: 1\nsource_session: s\n---\nBody {i}.\n"));
        }
        let entries = load_entries(&root);
        assert_eq!(entries.len(), 70); // loader reports disk truth; cap applies to catalog+admission
        let out = append_memory_in("BASE", &root);
        assert!(out.contains("t000"));
        assert!(out.contains(format!("t{:03}", MAX_ENTRIES - 1).as_str()));
        assert!(!out.contains("t064"));
    }

    #[test]
    fn byte_stable_for_same_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("memory");
        std::fs::create_dir_all(&root).unwrap();
        write(
            &root,
            "a.md",
            "---\ntitle: t\ntags: [x]\ncreated: 1\nsource_session: s\n---\nBody.\n",
        );
        assert_eq!(append_memory_in("B", &root), append_memory_in("B", &root));
    }

    #[test]
    fn duplicate_title_deterministic() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("memory");
        std::fs::create_dir_all(&root).unwrap();
        write(
            &root,
            "b.md",
            "---\ntitle: dup\ntags: [b]\ncreated: 1\nsource_session: s\n---\nFrom b.\n",
        );
        write(
            &root,
            "a.md",
            "---\ntitle: dup\ntags: [a]\ncreated: 1\nsource_session: s\n---\nFrom a.\n",
        );
        let entries = load_entries(&root);
        assert_eq!(entries.len(), 1);
        // Smaller source path wins regardless of read_dir order.
        assert_eq!(entries[0].source, root.join("a.md"));
        assert_eq!(entries[0].tags, vec!["a".to_string()]);
        let first = append_memory_in("BASE", &root);
        assert_eq!(first, append_memory_in("BASE", &root));
    }

    #[test]
    fn entry_markdown_roundtrips() {
        let md = entry_markdown("t", &["a".into(), "b".into()], "1", "s1", "Body text.");
        assert!(md.contains("title: t"));
        assert!(md.contains("tags: [a, b]"));
        assert!(md.contains("created: 1"));
        assert!(md.contains("source_session: s1"));
        assert!(md.ends_with("Body text.\n"));
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("memory");
        std::fs::create_dir_all(&root).unwrap();
        write(&root, "t.md", &md);
        let e = &load_entries(&root)[0];
        assert_eq!((e.title.as_str(), e.preview.as_str()), ("t", "Body text."));
    }

    // ---- append_memory (production entry) ----

    #[test]
    fn append_memory_production_entry_reads_the_config_dir() {
        // The production entry points at the machine's real config dir
        // (~/.conga/memory), which cannot be redirected without mutating the
        // process environment. Assert the structural contract instead: the
        // base prompt survives and any appended catalog is well-formed.
        let out = append_memory("BASE");
        assert!(out.starts_with("BASE"), "{out}");
        if out.len() > "BASE".len() {
            assert!(out.contains("## Memory"), "{out}");
            assert!(out.contains("(source: "), "{out}");
            assert!(out.contains("`read`"), "{out}");
        }
    }

    // ---- catalog_entries ----

    #[test]
    fn catalog_entries_is_empty_for_a_missing_root() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(catalog_entries(&tmp.path().join("memory")).is_empty());
    }

    #[test]
    fn catalog_entries_is_sorted_by_title() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("memory");
        std::fs::create_dir_all(&root).unwrap();
        write(
            &root,
            "b.md",
            "---\ntitle: beta\ntags: [t]\ncreated: 1\nsource_session: s\n---\nBody b.\n",
        );
        write(
            &root,
            "a.md",
            "---\ntitle: alpha\ntags: [t]\ncreated: 1\nsource_session: s\n---\nBody a.\n",
        );
        let titles: Vec<String> = catalog_entries(&root).keys().cloned().collect();
        assert_eq!(titles, vec!["alpha".to_string(), "beta".to_string()]);
    }

    #[test]
    fn catalog_entries_is_capped_at_max_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("memory");
        std::fs::create_dir_all(&root).unwrap();
        for i in 0..(MAX_ENTRIES + 6) {
            write(
                &root,
                format!("{i:03}.md").as_str(),
                &format!(
                    "---\ntitle: t{i:03}\ntags: [t]\ncreated: 1\nsource_session: s\n---\nBody {i}.\n"
                ),
            );
        }
        let entries = catalog_entries(&root);
        assert_eq!(entries.len(), MAX_ENTRIES, "the catalog is capped");
        // The cap keeps the first (sorted) titles.
        assert!(entries.contains_key("t000"));
        assert!(!entries.contains_key(&format!("t{:03}", MAX_ENTRIES)));
    }

    // ---- parse_entry ----

    #[test]
    fn parse_entry_reads_title_tags_and_preview() {
        assert_eq!(
            parse_entry("---\ntitle: t\ntags: [a, b]\n---\nFirst line.\nsecond line\n"),
            Some((
                "t".to_string(),
                vec!["a".to_string(), "b".to_string()],
                "First line.".to_string()
            ))
        );
    }

    #[test]
    fn parse_entry_requires_a_non_empty_title() {
        assert_eq!(
            parse_entry("---\ntags: [a]\n---\nbody\n"),
            None,
            "a missing title skips the entry"
        );
        assert_eq!(
            parse_entry("---\ntitle:\ntags: [a]\n---\nbody\n"),
            None,
            "an empty title skips the entry"
        );
    }

    #[test]
    fn parse_entry_rejects_a_missing_or_unterminated_frontmatter_block() {
        assert_eq!(parse_entry("no frontmatter at all"), None);
        assert_eq!(parse_entry("---\ntitle: t\ntags: [a]\n"), None);
    }

    #[test]
    fn parse_entry_defaults_missing_tags_to_an_empty_list() {
        let (title, tags, _) = parse_entry("---\ntitle: t\n---\nbody\n").unwrap();
        assert_eq!(title, "t");
        assert!(tags.is_empty());
    }

    #[test]
    fn parse_entry_strips_quotes_from_the_title_and_tags() {
        let (title, tags, _) =
            parse_entry("---\ntitle: \"quoted\"\ntags: [\"a\", 'b']\n---\nbody\n").unwrap();
        assert_eq!(title, "quoted");
        assert_eq!(tags, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn parse_entry_preview_is_the_first_non_empty_body_line() {
        let (_, _, preview) =
            parse_entry("---\ntitle: t\n---\n\n   \n  first line  \nsecond\n").unwrap();
        assert_eq!(preview, "first line");
    }

    #[test]
    fn parse_entry_preview_is_empty_without_a_body() {
        let (_, _, preview) = parse_entry("---\ntitle: t\n---\n").unwrap();
        assert_eq!(preview, "");
    }

    #[test]
    fn parse_entry_preview_is_bounded() {
        let long = "x".repeat(MAX_PREVIEW_CHARS + 40);
        let (_, _, preview) = parse_entry(&format!("---\ntitle: t\n---\n{long}\n")).unwrap();
        assert_eq!(preview.chars().count(), MAX_PREVIEW_CHARS + 1);
        assert!(preview.ends_with('…'));
    }

    // ---- bound_chars ----

    #[test]
    fn bound_chars_keeps_a_short_string_unchanged() {
        assert_eq!(bound_chars("abc", 10), "abc");
        assert_eq!(bound_chars("", 10), "");
    }

    #[test]
    fn bound_chars_truncates_and_appends_an_ellipsis() {
        assert_eq!(bound_chars("abcdef", 3), "abc…");
    }

    #[test]
    fn bound_chars_at_the_limit_is_unchanged() {
        assert_eq!(bound_chars("abc", 3), "abc");
    }

    #[test]
    fn bound_chars_does_not_split_a_multibyte_char() {
        // A byte-wise truncation would panic or produce invalid UTF-8 here.
        let out = bound_chars("中文测试", 3);
        assert_eq!(out, "中文测…");
    }

    #[test]
    fn bound_chars_with_a_zero_limit_is_just_the_ellipsis() {
        assert_eq!(bound_chars("abc", 0), "…");
    }

    // ---- unquote ----

    #[test]
    fn unquote_removes_one_balanced_quote_pair() {
        assert_eq!(unquote("\"abc\""), "abc");
        assert_eq!(unquote("'abc'"), "abc");
    }

    #[test]
    fn unquote_keeps_unbalanced_or_mismatched_quotes() {
        assert_eq!(unquote("\"abc"), "\"abc");
        assert_eq!(unquote("abc\""), "abc\"");
        assert_eq!(unquote("\"abc'"), "\"abc'");
    }

    #[test]
    fn unquote_of_an_empty_string_is_empty() {
        assert_eq!(unquote(""), "");
    }

    #[test]
    fn unquote_strips_only_the_outermost_pair() {
        assert_eq!(unquote("\"\"abc\"\""), "\"abc\"");
    }
}
