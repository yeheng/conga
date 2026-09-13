//! Directory input source: enumeration with include/exclude globs.

use std::path::{Path, PathBuf};

use anyhow::Context;
use globset::{Glob, GlobSet, GlobSetBuilder};

use crate::config::SourceConfig;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct FileEntry {
    /// Absolute path on disk.
    pub path: PathBuf,
    /// Path relative to the source root, '/'-separated (glob target).
    pub rel: String,
}
#[derive(Debug)]
pub struct DirSource {
    name: String,
    root: PathBuf,
    include: Option<GlobSet>,
    exclude: GlobSet,
}

fn build_globs(patterns: &[String]) -> anyhow::Result<Option<GlobSet>> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut b = GlobSetBuilder::new();
    for p in patterns {
        b.add(Glob::new(p).with_context(|| format!("非法 glob: {p}"))?);
    }
    Ok(Some(b.build()?))
}

impl DirSource {
    pub fn new(name: &str, cfg: &SourceConfig) -> anyhow::Result<DirSource> {
        anyhow::ensure!(
            cfg.path.exists(),
            "源 {name} 目录不存在: {}",
            cfg.path.display()
        );
        Ok(DirSource {
            name: name.to_string(),
            root: cfg.path.clone(),
            include: build_globs(&cfg.include)?,
            exclude: build_globs(&cfg.exclude)?.unwrap_or_default(),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn scan(&self) -> anyhow::Result<Vec<FileEntry>> {
        let mut out = Vec::new();
        for entry in ignore::WalkBuilder::new(&self.root)
            .require_git(false)
            .build()
        {
            let entry = entry.with_context(|| format!("遍历 {} 失败", self.root.display()))?;
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let rel = entry
                .path()
                .strip_prefix(&self.root)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, "/");
            if let Some(inc) = &self.include {
                if !inc.is_match(&rel) {
                    continue;
                }
            }
            if !self.exclude.is_empty() && self.exclude.is_match(&rel) {
                continue;
            }
            out.push(FileEntry {
                path: entry.path().to_path_buf(),
                rel,
            });
        }
        out.sort();
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(path: &Path, include: &[&str], exclude: &[&str]) -> SourceConfig {
        SourceConfig {
            kind: "dir".into(),
            path: path.to_path_buf(),
            include: include.iter().map(|s| s.to_string()).collect(),
            exclude: exclude.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn scans_matching_files_with_rel_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("a.md"), "x").unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/b.md"), "x").unwrap();
        std::fs::write(root.join("c.txt"), "x").unwrap();
        let src = DirSource::new("notes", &cfg(root, &["**/*.md"], &[])).unwrap();
        let mut files: Vec<String> = src.scan().unwrap().into_iter().map(|f| f.rel).collect();
        files.sort();
        assert_eq!(files, vec!["a.md", "sub/b.md"]);
    }

    #[test]
    fn empty_include_matches_everything() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("anything.bin"), "x").unwrap();
        let src = DirSource::new("s", &cfg(dir.path(), &[], &[])).unwrap();
        assert_eq!(src.scan().unwrap().len(), 1);
    }

    #[test]
    fn exclude_wins_over_include() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("drafts")).unwrap();
        std::fs::write(root.join("keep.md"), "x").unwrap();
        std::fs::write(root.join("drafts/skip.md"), "x").unwrap();
        let src = DirSource::new("s", &cfg(root, &["**/*.md"], &["**/drafts/**"])).unwrap();
        let files: Vec<String> = src.scan().unwrap().into_iter().map(|f| f.rel).collect();
        assert_eq!(files, vec!["keep.md"]);
    }

    #[test]
    fn gitignore_is_respected() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".gitignore"), "ignored.md\n").unwrap();
        std::fs::write(root.join("ignored.md"), "x").unwrap();
        std::fs::write(root.join("kept.md"), "x").unwrap();
        let src = DirSource::new("s", &cfg(root, &["**/*.md"], &[])).unwrap();
        let files: Vec<String> = src.scan().unwrap().into_iter().map(|f| f.rel).collect();
        assert_eq!(files, vec!["kept.md"]);
    }

    #[test]
    fn missing_root_is_error() {
        let err =
            DirSource::new("s", &cfg(Path::new("/nonexistent-xyz-123"), &[], &[])).unwrap_err();
        assert!(
            err.to_string().contains("不存在") || err.to_string().contains("not exist"),
            "{err}"
        );
    }
}
