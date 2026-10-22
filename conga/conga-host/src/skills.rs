//! Skills: a prompt-appended catalog of on-disk instruction files.
//!
//! Global skills live at `<config_dir>/skills/*.md`; project skills at
//! `<cwd>/.conga/skills/*.md` (same `name:` wins over the global one).
//! Only the catalog line + readable path is appended — the model reads a
//! skill's full content with the `read` tool on demand. Global entries carry
//! an absolute path (under `~/.conga`, which `read` accepts); project
//! entries carry a cwd-relative path (`read` resolves those within cwd).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Catalog descriptions are capped at this many chars: the catalog rides in
/// the system prompt (paid on every request), so a runaway frontmatter
/// description must not leak skill bodies into it.
const MAX_DESCRIPTION_CHARS: usize = 200;

struct SkillMeta {
    description: String,
    source: PathBuf,
}

/// Production entry: global root is conga's config dir (`~/.conga`).
pub fn append_skills(base: &str, cwd: &Path) -> String {
    append_skills_in(base, cwd, &conga::storage::config_dir())
}

/// Testable core: `global_root` is injected (production uses the config dir).
pub fn append_skills_in(base: &str, cwd: &Path, global_root: &Path) -> String {
    let catalog = scan_catalog(cwd, global_root);
    if catalog.is_empty() {
        return base.to_string();
    }
    let mut out = String::from(base);
    out.push_str(
        "\n\n## Skills\n\nThe entries below are skills. To follow one, load its \
         full instructions with the `read` tool using the path shown for it.\n\n",
    );
    for (name, meta) in &catalog {
        out.push_str(&format!(
            "- name: {name} — {} (source: {})\n",
            catalog_description(&meta.description),
            meta.source.display()
        ));
    }
    out
}

/// Shared scan: global skills first, then project skills (same `name:`
/// wins over the global one).
fn scan_catalog(cwd: &Path, global_root: &Path) -> BTreeMap<String, SkillMeta> {
    let mut catalog: BTreeMap<String, SkillMeta> = BTreeMap::new();
    scan_dir(&global_root.join("skills"), None, &mut catalog);
    scan_dir(&cwd.join(".conga").join("skills"), Some(cwd), &mut catalog);
    catalog
}

/// (name, description) pairs for consumers that need the catalog without
/// prompt formatting (evolve's extractor input). Same scan + override
/// semantics as `append_skills_in`.
pub(crate) fn catalog_entries(cwd: &Path, global_root: &Path) -> Vec<(String, String)> {
    scan_catalog(cwd, global_root)
        .into_iter()
        .map(|(name, meta)| (name, meta.description))
        .collect()
}

fn scan_dir(dir: &Path, relative_to: Option<&Path>, catalog: &mut BTreeMap<String, SkillMeta>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return, // no dir = no skills; not an error
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        match std::fs::read_to_string(&path)
            .ok()
            .and_then(|c| parse_frontmatter(&c))
        {
            Some((name, description)) => {
                // Project skills are stored cwd-relative: `read` resolves
                // relative paths within cwd, while absolute paths are only
                // allowed under the conga config dir. Global skills keep
                // the absolute path.
                let source = relative_to
                    .and_then(|base| path.strip_prefix(base).ok())
                    .unwrap_or(&path)
                    .to_path_buf();
                catalog.insert(
                    name,
                    SkillMeta {
                        description,
                        source,
                    },
                );
            }
            None => {
                tracing::warn!(path = %path.display(), "skill missing frontmatter name/description; skipped")
            }
        }
    }
}

/// One-line, bounded description for the catalog line.
fn catalog_description(d: &str) -> String {
    if d.chars().count() <= MAX_DESCRIPTION_CHARS {
        return d.replace(['\n', '\r'], " ");
    }
    let mut t: String = d.chars().take(MAX_DESCRIPTION_CHARS).collect();
    t.push('…');
    t
}

/// Hand-parsed frontmatter: `---\n` … `---` block carrying both `name:` and
/// `description:` lines. `description` may use a block scalar (`|`/`>` with
/// optional `-`/`+` chomping) — the indented lines that follow are collapsed
/// to one line for the catalog. Anything else is None (caller warns + skips).
fn parse_frontmatter(content: &str) -> Option<(String, String)> {
    let mut name = None;
    let mut description = None;
    let mut lines = content.strip_prefix("---\n")?.lines().peekable();
    while let Some(line) = lines.next() {
        if line.trim_end() == "---" {
            return match (name, description) {
                (Some(n), Some(d)) => Some((n, d)),
                _ => None,
            };
        }
        if let Some(v) = line.strip_prefix("name:") {
            name = parse_value(v, &mut lines);
        } else if let Some(v) = line.strip_prefix("description:") {
            description = parse_value(v, &mut lines);
        }
    }
    None // unterminated frontmatter block
}

/// Value of a frontmatter key: a plain single-line scalar, or a `|`/`>`
/// block scalar (the indented lines that follow) collapsed to one line.
/// `None` for an empty value.
fn parse_value(v: &str, lines: &mut std::iter::Peekable<std::str::Lines<'_>>) -> Option<String> {
    let v = v.trim();
    // `|`/`>` optionally followed by a chomping indicator (`-`/`+`) opens a
    // block scalar; anything else after the indicator (e.g. `|2`) is treated
    // as a plain scalar.
    let block_scalar = matches!(v.as_bytes().first(), Some(b'|') | Some(b'>'))
        && v[1..].bytes().all(|b| b == b'-' || b == b'+');
    if !block_scalar {
        let scalar = unquote(v);
        return (!scalar.is_empty()).then(|| scalar.to_string());
    }
    let mut parts: Vec<&str> = Vec::new();
    while let Some(l) = lines.peek() {
        if l.starts_with(' ') || l.starts_with('\t') {
            let trimmed = l.trim();
            if !trimmed.is_empty() {
                parts.push(trimmed);
            }
            lines.next();
        } else if l.trim().is_empty() {
            lines.next(); // blank line inside a block adds nothing once collapsed
        } else {
            break; // dedent: the block ends, this line is a normal entry
        }
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// Strip one balanced pair of surrounding quotes from a plain scalar
/// (`"my-skill"` / `'my-skill'`): editors and project templates routinely
/// emit quoted frontmatter, and the quotes would otherwise pollute the
/// catalog line (and the tool name the model matches against). Only a
/// matching pair at both ends is removed - a bare quote mid-value stays
/// part of the scalar.
fn unquote(v: &str) -> &str {
    let b = v.as_bytes();
    if b.len() >= 2
        && ((b[0] == b'"' && b[b.len() - 1] == b'"') || (b[0] == b'\'' && b[b.len() - 1] == b'\''))
    {
        &v[1..v.len() - 1]
    } else {
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn skill_file(dir: &Path, file: &str, body: &str) {
        fs::create_dir_all(dir.join("skills")).unwrap();
        fs::write(dir.join("skills").join(file), body).unwrap();
    }

    const OK: &str =
        "---\nname: code-review\ndescription: Review diffs for bugs\n---\nBody never injected.\n";

    #[test]
    fn appends_catalog_for_valid_skill() {
        let tmp = tempfile::tempdir().unwrap();
        skill_file(tmp.path(), "review.md", OK);
        let out = append_skills_in("BASE", Path::new("/nope"), tmp.path());
        assert!(out.starts_with("BASE"));
        assert!(out.contains("## Skills"));
        assert!(out.contains("- name: code-review — Review diffs for bugs"));
        assert!(out.contains(&format!(
            "(source: {})",
            tmp.path().join("skills/review.md").display()
        )));
        assert!(
            out.contains("`read`"),
            "must tell the model to use the read tool"
        );
        assert!(!out.contains("Body never injected"));
    }

    #[test]
    fn missing_description_and_no_frontmatter_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        skill_file(tmp.path(), "bad.md", "---\nname: x\n---\nbody\n");
        assert_eq!(
            append_skills_in("BASE", Path::new("/nope"), tmp.path()),
            "BASE"
        );
        skill_file(tmp.path(), "plain.md", "Just some markdown.\n");
        assert_eq!(
            append_skills_in("BASE", Path::new("/nope"), tmp.path()),
            "BASE"
        );
    }

    #[test]
    fn no_skills_returns_base_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            append_skills_in("BASE", Path::new("/nope"), tmp.path()),
            "BASE"
        );
    }

    #[test]
    fn project_overrides_global_on_name_clash() {
        let g = tempfile::tempdir().unwrap();
        let p = tempfile::tempdir().unwrap();
        skill_file(
            g.path(),
            "a.md",
            "---\nname: dup\ndescription: global one\n---\n",
        );
        skill_file(
            &p.path().join(".conga"),
            "b.md",
            "---\nname: dup\ndescription: project one\n---\n",
        );
        let out = append_skills_in("BASE", p.path(), g.path());
        assert!(out.contains("project one") && !out.contains("global one"));
        assert_eq!(
            out.matches("- name: dup").count(),
            1,
            "exactly one line per name"
        );
    }

    #[test]
    fn project_only_skill_is_included() {
        let g = tempfile::tempdir().unwrap();
        let p = tempfile::tempdir().unwrap();
        skill_file(
            &p.path().join(".conga"),
            "only.md",
            "---\nname: proj\ndescription: from project\n---\n",
        );
        assert!(
            append_skills_in("BASE", p.path(), g.path()).contains("- name: proj — from project")
        );
    }

    #[test]
    fn project_skill_source_is_cwd_relative() {
        let g = tempfile::tempdir().unwrap();
        let p = tempfile::tempdir().unwrap();
        skill_file(g.path(), "glob.md", OK);
        skill_file(
            &p.path().join(".conga"),
            "proj.md",
            "---\nname: proj\ndescription: from project\n---\n",
        );
        let out = append_skills_in("BASE", p.path(), g.path());
        // Project entries must be cwd-relative: `read` rejects absolute
        // paths outside `~/.conga`, so the catalog must hand the model a
        // path it can actually use.
        assert!(
            out.contains("(source: .conga/skills/proj.md)"),
            "project source should be cwd-relative, got: {out}"
        );
        assert!(
            out.contains(&format!(
                "(source: {})",
                g.path().join("skills/glob.md").display()
            )),
            "global source should stay absolute"
        );
    }

    #[test]
    fn quoted_scalars_are_unquoted() {
        let tmp = tempfile::tempdir().unwrap();
        skill_file(
            tmp.path(),
            "quoted.md",
            "---\nname: \"code-review\"\ndescription: 'Review diffs for bugs'\n---\nbody\n",
        );
        let out = append_skills_in("BASE", Path::new("/nope"), tmp.path());
        assert!(
            // (em dash separator - matches the catalog format above)
            out.contains("- name: code-review — Review diffs for bugs"),
            "quotes must be stripped for both keys, got: {out}"
        );
    }

    #[test]
    fn unbalanced_quotes_stay_part_of_scalar() {
        let tmp = tempfile::tempdir().unwrap();
        skill_file(
            tmp.path(),
            "odd.md",
            "---\nname: it's-fine\ndescription: say \"hi\" now\n---\n",
        );
        let out = append_skills_in("BASE", Path::new("/nope"), tmp.path());
        assert!(
            // (em dash separator - matches the catalog format above)
            out.contains("- name: it's-fine — say \"hi\" now"),
            "mid-value quotes must survive, got: {out}"
        );
    }

    #[test]
    fn literal_block_scalar_description_is_collapsed() {
        let tmp = tempfile::tempdir().unwrap();
        skill_file(
            tmp.path(),
            "block.md",
            "---\nname: blk\ndescription: |\n  First line of purpose.\n  Second line, more detail.\n---\nbody\n",
        );
        let out = append_skills_in("BASE", Path::new("/nope"), tmp.path());
        assert!(
            out.contains("- name: blk — First line of purpose. Second line, more detail."),
            "block scalar must collapse to one catalog line, got: {out}"
        );
    }

    #[test]
    fn folded_block_scalar_with_chomp_is_collapsed() {
        let tmp = tempfile::tempdir().unwrap();
        skill_file(
            tmp.path(),
            "fold.md",
            "---\nname: fold\ndescription: >-\n  folded one\n  folded two\n---\n",
        );
        let out = append_skills_in("BASE", Path::new("/nope"), tmp.path());
        assert!(
            out.contains("- name: fold — folded one folded two"),
            "folded scalar with chomping must collapse, got: {out}"
        );
    }

    #[test]
    fn long_description_is_truncated() {
        let tmp = tempfile::tempdir().unwrap();
        let long: String = "x".repeat(300);
        skill_file(
            tmp.path(),
            "long.md",
            &format!("---\nname: long\ndescription: {long}\n---\n"),
        );
        let out = append_skills_in("BASE", Path::new("/nope"), tmp.path());
        let expected: String = "x".repeat(MAX_DESCRIPTION_CHARS);
        assert!(
            out.contains(&format!("- name: long — {expected}…")),
            "description must be capped at {MAX_DESCRIPTION_CHARS} chars, got: {out}"
        );
        assert!(
            !out.contains(&"x".repeat(MAX_DESCRIPTION_CHARS + 1)),
            "no run longer than the cap may appear"
        );
    }

    // ---- append_skills (production entry) ----

    #[test]
    fn append_skills_production_entry_reads_the_config_dir() {
        // The production entry points at the machine's real config dir
        // (~/.conga), which cannot be redirected without mutating the
        // process environment. Assert the structural contract instead: the
        // base prompt survives and any appended catalog is well-formed.
        let out = append_skills("BASE", Path::new("/no-such-cwd-for-test"));
        assert!(out.starts_with("BASE"), "{out}");
        if out.len() > "BASE".len() {
            assert!(out.contains("## Skills"), "{out}");
            assert!(out.contains("- name: "), "{out}");
            assert!(out.contains("(source: "), "{out}");
            assert!(out.contains("`read`"), "{out}");
        }
    }

    // ---- scan_catalog ----

    #[test]
    fn scan_catalog_merges_global_and_project_skills() {
        let g = tempfile::tempdir().unwrap();
        let p = tempfile::tempdir().unwrap();
        skill_file(
            g.path(),
            "g.md",
            "---\nname: gskill\ndescription: from global\n---\n",
        );
        skill_file(
            &p.path().join(".conga"),
            "p.md",
            "---\nname: pskill\ndescription: from project\n---\n",
        );
        let catalog = scan_catalog(p.path(), g.path());
        assert_eq!(catalog.len(), 2);
        assert_eq!(catalog.get("gskill").unwrap().description, "from global");
        assert_eq!(catalog.get("pskill").unwrap().description, "from project");
    }

    #[test]
    fn scan_catalog_project_skill_overrides_the_global_one_of_the_same_name() {
        let g = tempfile::tempdir().unwrap();
        let p = tempfile::tempdir().unwrap();
        skill_file(
            g.path(),
            "dup.md",
            "---\nname: dup\ndescription: global one\n---\n",
        );
        skill_file(
            &p.path().join(".conga"),
            "dup.md",
            "---\nname: dup\ndescription: project one\n---\n",
        );
        let catalog = scan_catalog(p.path(), g.path());
        assert_eq!(catalog.len(), 1, "same name → one entry");
        assert_eq!(catalog.get("dup").unwrap().description, "project one");
        assert_eq!(
            catalog.get("dup").unwrap().source,
            PathBuf::from(".conga/skills/dup.md"),
            "the project entry wins, with a cwd-relative source"
        );
    }

    #[test]
    fn scan_catalog_of_missing_directories_is_empty() {
        let catalog = scan_catalog(Path::new("/no/such/cwd"), Path::new("/no/such/global"));
        assert!(
            catalog.is_empty(),
            "a missing dir means no skills, not an error"
        );
    }

    #[test]
    fn scan_catalog_ignores_non_markdown_files() {
        let g = tempfile::tempdir().unwrap();
        skill_file(
            g.path(),
            "notes.txt",
            "---\nname: txt\ndescription: nope\n---\n",
        );
        skill_file(g.path(), "ok.md", "---\nname: ok\ndescription: yes\n---\n");
        let catalog = scan_catalog(Path::new("/no/such/cwd"), g.path());
        assert_eq!(catalog.len(), 1);
        assert!(catalog.contains_key("ok"));
    }

    // ---- catalog_entries ----

    #[test]
    fn catalog_entries_returns_name_description_pairs_sorted_by_name() {
        let g = tempfile::tempdir().unwrap();
        skill_file(
            g.path(),
            "b.md",
            "---\nname: beta\ndescription: second\n---\n",
        );
        skill_file(
            g.path(),
            "a.md",
            "---\nname: alpha\ndescription: first\n---\n",
        );
        let entries = catalog_entries(Path::new("/no/such/cwd"), g.path());
        assert_eq!(
            entries,
            vec![
                ("alpha".to_string(), "first".to_string()),
                ("beta".to_string(), "second".to_string()),
            ],
            "the catalog is sorted by name (byte-stable prompt)"
        );
    }

    #[test]
    fn catalog_entries_of_an_empty_root_is_empty() {
        let g = tempfile::tempdir().unwrap();
        assert!(catalog_entries(Path::new("/no/such/cwd"), g.path()).is_empty());
    }

    // ---- scan_dir ----

    #[test]
    fn scan_dir_skips_files_without_usable_frontmatter() {
        let dir = tempfile::tempdir().unwrap().path().join("skills");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("bad.md"), "no frontmatter at all").unwrap();
        fs::write(dir.join("half.md"), "---\nname: half\n---\nbody\n").unwrap();
        fs::write(dir.join("skip.txt"), "not markdown").unwrap();
        fs::write(
            dir.join("ok.md"),
            "---\nname: ok\ndescription: fine\n---\nbody\n",
        )
        .unwrap();

        let mut catalog: BTreeMap<String, SkillMeta> = BTreeMap::new();
        scan_dir(&dir, None, &mut catalog);
        assert_eq!(catalog.len(), 1, "only the complete skill is cataloged");
        assert_eq!(catalog.get("ok").unwrap().description, "fine");
    }

    #[test]
    fn scan_dir_keeps_absolute_paths_when_no_base_is_given() {
        let dir = tempfile::tempdir().unwrap().path().join("skills");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("ok.md"), "---\nname: ok\ndescription: fine\n---\n").unwrap();
        let mut catalog: BTreeMap<String, SkillMeta> = BTreeMap::new();
        scan_dir(&dir, None, &mut catalog);
        assert_eq!(catalog.get("ok").unwrap().source, dir.join("ok.md"));
    }

    #[test]
    fn scan_dir_strips_the_base_path_for_project_entries() {
        let base = tempfile::tempdir().unwrap();
        let dir = base.path().join(".conga").join("skills");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("ok.md"), "---\nname: ok\ndescription: fine\n---\n").unwrap();
        let mut catalog: BTreeMap<String, SkillMeta> = BTreeMap::new();
        scan_dir(&dir, Some(base.path()), &mut catalog);
        assert_eq!(
            catalog.get("ok").unwrap().source,
            PathBuf::from(".conga/skills/ok.md"),
            "a project source must be cwd-relative so `read` can resolve it"
        );
    }

    #[test]
    fn scan_dir_of_a_missing_directory_is_a_no_op() {
        let mut catalog: BTreeMap<String, SkillMeta> = BTreeMap::new();
        scan_dir(Path::new("/no/such/skills"), None, &mut catalog);
        assert!(catalog.is_empty());
    }

    // ---- catalog_description ----

    #[test]
    fn catalog_description_keeps_a_short_description_as_is() {
        assert_eq!(catalog_description("short"), "short");
        assert_eq!(catalog_description(""), "");
    }

    #[test]
    fn catalog_description_flattens_newlines_into_spaces() {
        assert_eq!(catalog_description("a\nb"), "a b");
        assert_eq!(catalog_description("a\r\nb"), "a  b");
    }

    #[test]
    fn catalog_description_at_the_cap_is_not_truncated() {
        let exact = "x".repeat(MAX_DESCRIPTION_CHARS);
        assert_eq!(catalog_description(&exact), exact);
        assert!(!catalog_description(&exact).ends_with('…'));
    }

    #[test]
    fn catalog_description_over_the_cap_is_truncated_with_an_ellipsis() {
        let long = "x".repeat(MAX_DESCRIPTION_CHARS + 50);
        let out = catalog_description(&long);
        assert_eq!(
            out.chars().count(),
            MAX_DESCRIPTION_CHARS + 1,
            "the cap plus the ellipsis"
        );
        assert!(out.ends_with('…'));
        assert_eq!(
            out.chars().take(MAX_DESCRIPTION_CHARS).collect::<String>(),
            "x".repeat(MAX_DESCRIPTION_CHARS)
        );
    }

    #[test]
    fn catalog_description_counts_chars_not_bytes() {
        // 200 multi-byte chars are 600 bytes but still within the char cap.
        let s = "中".repeat(MAX_DESCRIPTION_CHARS);
        assert_eq!(catalog_description(&s), s);
        let over = "中".repeat(MAX_DESCRIPTION_CHARS + 1);
        assert_eq!(
            catalog_description(&over).chars().count(),
            MAX_DESCRIPTION_CHARS + 1
        );
    }

    // ---- parse_frontmatter ----

    #[test]
    fn parse_frontmatter_reads_name_and_description() {
        assert_eq!(
            parse_frontmatter("---\nname: a\ndescription: b\n---\nbody\n"),
            Some(("a".to_string(), "b".to_string()))
        );
    }

    #[test]
    fn parse_frontmatter_requires_both_keys() {
        assert_eq!(
            parse_frontmatter("---\nname: a\n---\nbody\n"),
            None,
            "a missing description skips the skill"
        );
        assert_eq!(
            parse_frontmatter("---\ndescription: b\n---\nbody\n"),
            None,
            "a missing name skips the skill"
        );
    }

    #[test]
    fn parse_frontmatter_rejects_a_missing_or_unterminated_block() {
        assert_eq!(parse_frontmatter("name: a\ndescription: b\n"), None);
        assert_eq!(parse_frontmatter("---\nname: a\ndescription: b\n"), None);
        assert_eq!(
            parse_frontmatter("---\r\nname: a\ndescription: b\n---\n"),
            None
        );
    }

    #[test]
    fn parse_frontmatter_must_start_the_file() {
        assert_eq!(
            parse_frontmatter("\n---\nname: a\ndescription: b\n---\n"),
            None,
            "frontmatter must be the very first thing in the file"
        );
    }

    #[test]
    fn parse_frontmatter_ignores_unknown_keys() {
        assert_eq!(
            parse_frontmatter("---\nfoo: bar\nname: a\ndescription: b\n---\n"),
            Some(("a".to_string(), "b".to_string()))
        );
    }

    #[test]
    fn parse_frontmatter_reads_a_block_scalar_description() {
        assert_eq!(
            parse_frontmatter("---\nname: blk\ndescription: |\n  one\n  two\n---\nbody\n"),
            Some(("blk".to_string(), "one two".to_string()))
        );
    }

    // ---- parse_value ----

    #[test]
    fn parse_value_reads_a_plain_scalar() {
        let mut lines = "".lines().peekable();
        assert_eq!(
            parse_value("  plain  ", &mut lines),
            Some("plain".to_string())
        );
    }

    #[test]
    fn parse_value_returns_none_for_an_empty_scalar() {
        let mut lines = "".lines().peekable();
        assert_eq!(parse_value("   ", &mut lines), None);
        assert_eq!(parse_value("", &mut lines), None);
    }

    #[test]
    fn parse_value_strips_one_quote_pair() {
        let mut lines = "".lines().peekable();
        assert_eq!(
            parse_value("\"quoted\"", &mut lines),
            Some("quoted".to_string())
        );
        assert_eq!(
            parse_value("'quoted'", &mut lines),
            Some("quoted".to_string())
        );
    }

    #[test]
    fn parse_value_collects_a_literal_block_scalar() {
        let mut lines = "  one\n  two\nname: next\n".lines().peekable();
        assert_eq!(parse_value("|", &mut lines), Some("one two".to_string()));
        assert_eq!(
            lines.next(),
            Some("name: next"),
            "the block ends at the dedent; the next entry is untouched"
        );
    }

    #[test]
    fn parse_value_collects_a_folded_block_scalar_with_a_chomp_indicator() {
        let mut lines = "  folded\n".lines().peekable();
        assert_eq!(parse_value(">-", &mut lines), Some("folded".to_string()));
        let mut lines = "  kept\n".lines().peekable();
        assert_eq!(parse_value("|+", &mut lines), Some("kept".to_string()));
    }

    #[test]
    fn parse_value_treats_an_indicator_with_a_digit_as_a_plain_scalar() {
        let mut lines = "".lines().peekable();
        assert_eq!(parse_value("|2", &mut lines), Some("|2".to_string()));
    }

    #[test]
    fn parse_value_block_scalar_without_content_is_none() {
        let mut lines = "name: next\n".lines().peekable();
        assert_eq!(parse_value("|", &mut lines), None);
    }

    #[test]
    fn parse_value_block_scalar_skips_blank_lines_inside_the_block() {
        let mut lines = "  one\n\n  two\n".lines().peekable();
        assert_eq!(parse_value("|", &mut lines), Some("one two".to_string()));
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
        assert_eq!(unquote("'abc\""), "'abc\"");
    }

    #[test]
    fn unquote_of_a_single_quote_character_is_unchanged() {
        assert_eq!(unquote("\""), "\"");
        assert_eq!(unquote("'"), "'");
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
