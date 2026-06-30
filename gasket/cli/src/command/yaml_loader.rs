//! Loads user-defined slash commands from `*.md` files with YAML front-matter.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use tracing::warn;

use crate::command::types::{Command, CommandKind};

#[derive(Deserialize)]
struct FrontMatter {
    name: String,
    description: String,
    #[serde(default)]
    aliases: Vec<String>,
    #[serde(default)]
    allowed_tools: Option<Vec<String>>,
}

pub async fn load_user_commands(dir: &Path) -> Vec<Command> {
    let mut entries: Vec<PathBuf> = match collect_md_paths(dir).await {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    entries.sort();
    let mut out = Vec::new();
    for path in entries {
        match load_one(&path).await {
            Ok(cmd) => out.push(cmd),
            Err(reason) => warn!(?path, reason, "skipping user command file"),
        }
    }
    out
}

async fn collect_md_paths(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    if !tokio::fs::try_exists(dir).await? {
        return Ok(vec![]);
    }
    let mut rd = tokio::fs::read_dir(dir).await?;
    let mut out = Vec::new();
    while let Some(entry) = rd.next_entry().await? {
        let p = entry.path();
        if p.extension().and_then(|s| s.to_str()) == Some("md") {
            out.push(p);
        }
    }
    Ok(out)
}

async fn load_one(path: &Path) -> Result<Command, String> {
    let raw = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| format!("read failed: {e}"))?;
    let (front, body) = split_front_matter(&raw).ok_or("missing front-matter")?;
    let fm: FrontMatter = serde_yaml::from_str(front).map_err(|e| format!("yaml parse: {e}"))?;
    if fm.name.trim().is_empty() {
        return Err("name field is empty".into());
    }
    if fm.description.trim().is_empty() {
        return Err("description field is empty".into());
    }
    Ok(Command {
        name: fm.name,
        description: fm.description,
        aliases: fm.aliases,
        kind: CommandKind::Yaml {
            prompt_template: body.trim_start().to_string(),
            allowed_tools: fm.allowed_tools,
        },
    })
}

fn split_front_matter(raw: &str) -> Option<(&str, &str)> {
    let stripped = raw.strip_prefix("---")?;
    let stripped = stripped
        .strip_prefix('\n')
        .or_else(|| stripped.strip_prefix("\r\n"))?;
    let end = stripped.find("\n---")?;
    let front = &stripped[..end];
    let after = &stripped[end + 4..];
    let body = after
        .strip_prefix('\n')
        .or_else(|| after.strip_prefix("\r\n"))
        .unwrap_or(after);
    Some((front, body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    async fn write(dir: &TempDir, name: &str, content: &str) -> PathBuf {
        let p = dir.path().join(name);
        tokio::fs::write(&p, content).await.unwrap();
        p
    }

    fn good_translate() -> &'static str {
        "---\n\
name: translate\n\
description: Translate text to Mandarin\n\
aliases: [tr]\n\
allowed_tools: []\n\
---\n\
\n\
Translate the following:\n\
{{user_input}}\n"
    }

    #[tokio::test]
    async fn loads_valid_command() {
        let dir = TempDir::new().unwrap();
        write(&dir, "translate.md", good_translate()).await;

        let cmds = load_user_commands(dir.path()).await;

        assert_eq!(cmds.len(), 1);
        let c = &cmds[0];
        assert_eq!(c.name, "translate");
        assert_eq!(c.aliases, vec!["tr".to_string()]);
        match &c.kind {
            CommandKind::Yaml {
                prompt_template,
                allowed_tools,
            } => {
                assert!(prompt_template.contains("{{user_input}}"));
                assert_eq!(allowed_tools, &Some(vec![]));
            }
            _ => panic!("expected Yaml kind"),
        }
    }

    #[tokio::test]
    async fn skips_broken_yaml() {
        let dir = TempDir::new().unwrap();
        write(&dir, "broken.md", "---\nthis: is: bad: yaml\n---\nbody\n").await;

        let cmds = load_user_commands(dir.path()).await;
        assert_eq!(cmds.len(), 0);
    }

    #[tokio::test]
    async fn skips_missing_name() {
        let dir = TempDir::new().unwrap();
        write(&dir, "no-name.md", "---\ndescription: x\n---\nbody\n").await;

        let cmds = load_user_commands(dir.path()).await;
        assert_eq!(cmds.len(), 0);
    }

    #[tokio::test]
    async fn skips_missing_front_matter() {
        let dir = TempDir::new().unwrap();
        write(&dir, "plain.md", "no front matter here\n").await;

        let cmds = load_user_commands(dir.path()).await;
        assert_eq!(cmds.len(), 0);
    }

    #[tokio::test]
    async fn ignores_non_md_files() {
        let dir = TempDir::new().unwrap();
        write(&dir, "translate.md", good_translate()).await;
        write(&dir, "notes.txt", good_translate()).await;
        write(&dir, "README", good_translate()).await;

        let cmds = load_user_commands(dir.path()).await;
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, "translate");
    }

    #[tokio::test]
    async fn empty_dir_returns_empty_vec() {
        let dir = TempDir::new().unwrap();
        let cmds = load_user_commands(dir.path()).await;
        assert!(cmds.is_empty());
    }

    #[tokio::test]
    async fn missing_dir_returns_empty_vec_silently() {
        let dir = TempDir::new().unwrap();
        let nope = dir.path().join("does-not-exist");
        let cmds = load_user_commands(&nope).await;
        assert!(cmds.is_empty());
    }

    #[tokio::test]
    async fn lex_order_is_deterministic() {
        let dir = TempDir::new().unwrap();
        write(&dir, "z-zulu.md", &swap_name(good_translate(), "zulu")).await;
        write(&dir, "a-alpha.md", &swap_name(good_translate(), "alpha")).await;
        write(&dir, "m-mike.md", &swap_name(good_translate(), "mike")).await;

        let names: Vec<String> = load_user_commands(dir.path())
            .await
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(names, vec!["alpha", "mike", "zulu"]);
    }

    fn swap_name(template: &str, new_name: &str) -> String {
        template.replace("name: translate", &format!("name: {new_name}"))
    }
}
