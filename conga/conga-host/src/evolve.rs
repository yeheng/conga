//! `/evolve` — distill a session transcript into memory insights and
//! skills, admitted one-by-one through the human approver. The read side
//! (`memory.rs`) only ever catalogs; everything here is the write side.

use conga::types::message::{AgentMessage, ContentBlock};
use serde::Deserialize;
use std::path::Path;
use std::sync::Arc;

use crate::permission::PermissionPolicy;
use crate::session::SessionManager;
use crate::subagent_types::{SubagentSpawn, SubagentSpawner};

/// Render derived messages to compact extraction input. Oldest messages
/// are dropped first when over budget (the freshest context — where the
/// mistake and its correction live — always survives), and the truncation
/// is flagged so the extractor knows the transcript has a hole.
pub fn render_trajectory(messages: &[AgentMessage], max_chars: usize) -> String {
    let mut blocks: Vec<String> = Vec::with_capacity(messages.len());
    for m in messages {
        match m {
            AgentMessage::User(u) => {
                for b in &u.content {
                    if let ContentBlock::Text { text } = b {
                        blocks.push(format!("## USER\n{text}"));
                    }
                }
            }
            AgentMessage::Assistant(a) => {
                let mut out = String::new();
                for b in &a.content {
                    match b {
                        ContentBlock::Thinking { .. } => {}
                        ContentBlock::Text { text } => {
                            if !text.trim().is_empty() {
                                out.push_str(&format!("## ASSISTANT\n{text}"));
                            }
                        }
                        ContentBlock::ToolCall { tool_call } => {
                            out.push_str(&format!(
                                "\n- tool call: {}({})\n",
                                tool_call.function.name,
                                bound(&tool_call.function.arguments, 200)
                            ));
                        }
                    }
                }
                if !out.trim().is_empty() {
                    blocks.push(out.trim().to_string());
                }
            }
            AgentMessage::ToolResult(r) => {
                for b in &r.content {
                    if let ContentBlock::Text { text } = b {
                        blocks.push(format!(
                            "## TOOL RESULT ({})\n{}",
                            r.tool_name,
                            bound(text, 2_000)
                        ));
                    }
                }
            }
            AgentMessage::Custom(_) => {}
        }
    }
    // Budget: drop whole oldest blocks until the joined text fits.
    let mut start = 0;
    loop {
        let joined = blocks[start..].join("\n\n");
        if joined.chars().count() <= max_chars || start >= blocks.len() {
            if start == 0 {
                return joined;
            }
            return format!("(older messages truncated — {start} blocks dropped)\n\n{joined}");
        }
        start += 1;
    }
}

/// Char-boundary-safe truncation.
fn bound(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max).collect();
    t.push('…');
    t
}

#[derive(Debug, Default, Deserialize)]
pub struct EvolveProposal {
    #[serde(default)]
    pub insights: Vec<InsightProposal>,
    #[serde(default)]
    pub skills: Vec<SkillProposal>,
    #[serde(default)]
    pub retires: Vec<String>,
    #[serde(default)]
    pub duplicates: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct InsightProposal {
    pub title: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub content: String,
}

#[derive(Debug, Deserialize)]
pub struct SkillProposal {
    pub name: String,
    pub description: String,
    pub body: String,
}

/// The one prompt the extraction sub-agent sees. The quality contract is
/// the point: insights must be root-cause + applied fix + evidence, or
/// the library fills with platitudes that get injected into every future
/// prompt (see docs/evolve.md "Content quality").
pub fn extraction_task_prompt(trajectory: &str, catalog: &str) -> String {
    format!(
        "You are a distillation engine for a coding assistant. You get ONE \
session transcript. Extract reusable knowledge for FUTURE sessions. Reply \
with ONLY a JSON object — no prose, no markdown fences.\n\n\
Rules for \"insights\":\n\
- Only what THIS session proved: each insight must state the root cause, the \
fix actually applied, and the evidence (what happened in the transcript).\n\
- No general advice (\"read errors carefully\"), no restating the task.\n\
- tags: 2-5 lowercase single-word tokens likely to appear in future tasks.\n\
- content: <= 2KB, imperative, self-contained.\n\n\
Rules for \"skills\": repeatable multi-step procedures actually demonstrated \
in the transcript; name = kebab-case; description = one line saying when to \
use it.\n\
Rules for \"retires\": titles from the existing library that this session \
proved obsolete or wrong.\n\
Rules for \"duplicates\": existing titles your new entries would duplicate — \
list them here instead of re-proposing them.\n\n\
Existing library:\n{catalog}\n\n\
Transcript:\n{trajectory}\n\n\
Output schema:\n\
{{\"insights\":[{{\"title\":\"\",\"tags\":[\"\"],\"content\":\"\"}}],\
\"skills\":[{{\"name\":\"\",\"description\":\"\",\"body\":\"\"}}],\
\"retires\":[\"\"],\"duplicates\":[\"\"]}}"
    )
}

/// Parse the extractor's reply: take the outermost {...} span so prose or
/// markdown fences around the JSON are tolerated; fail loud otherwise —
/// a silently empty proposal would look like "nothing to learn". A `}`
/// in prose before the first `{` closes nothing: it counts as absent so
/// an inverted span is an Err, never a slice panic.
pub fn parse_proposal(output: &str) -> Result<EvolveProposal, conga::AgentError> {
    let start = output.find('{').ok_or_else(|| {
        conga::AgentError::Tool(format!(
            "extractor output has no JSON object: {}",
            bound(output, 200)
        ))
    })?;
    let end = output
        .rfind('}')
        .filter(|&e| e >= start)
        .ok_or_else(|| conga::AgentError::Tool("extractor output has no closing brace".into()))?;
    let json = &output[start..=end];
    serde_json::from_str(json).map_err(conga::AgentError::Serde)
}

/// Library snapshot for the extractor input: every existing memory entry
/// and skill, so it proposes deltas rather than echoes.
pub fn catalog_snapshot(memory_root: &Path, cwd: &Path, global_root: &Path) -> String {
    let mut out = String::new();
    for e in crate::memory::load_entries(memory_root) {
        out.push_str(&format!("memory: {} [{}]\n", e.title, e.tags.join(", ")));
    }
    for (name, desc) in crate::skills::catalog_entries(cwd, global_root) {
        out.push_str(&format!("skill: {name} — {desc}\n"));
    }
    out
}

#[derive(Debug, Default)]
pub struct EvolveOutcome {
    pub added_insights: Vec<String>,
    pub added_skills: Vec<String>,
    pub updated_skills: Vec<String>,
    pub retired: Vec<String>,
    pub rejected: Vec<String>,
    pub skipped: Vec<String>,
}

impl EvolveOutcome {
    /// One-line human summary (CLI /evolve, tool result).
    pub fn summarize(&self) -> String {
        format!(
            "evolve: +{} insights, +{} skills, ~{} skills updated, -{} retired, {} rejected, {} skipped",
            self.added_insights.len(),
            self.added_skills.len(),
            self.updated_skills.len(),
            self.retired.len(),
            self.rejected.len(),
            self.skipped.len(),
        )
    }
}

/// Kebab-case file-safe name (skill proposal names are model output).
fn slug(name: &str) -> String {
    let mut s: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    s.trim_matches('-').to_string()
}

fn title_slug(title: &str) -> String {
    slug(title)
}

/// Admit every proposal: retires first (they free cap slots), then adds.
/// Each write/delete is individually approved; a rejection drops only
/// that candidate. The 64-entry cap is enforced HERE, at admission —
/// with the library full, an add lands only if a same-run retire freed
/// a slot.
pub async fn apply_proposals(
    proposal: &EvolveProposal,
    memory_root: &Path,
    skills_root: &Path,
    source_session: &str,
    policy: &PermissionPolicy,
) -> EvolveOutcome {
    let mut out = EvolveOutcome::default();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .to_string();

    // 1. Retires (approved removals free slots for the adds below).
    for title in &proposal.retires {
        if let Some(entry) = crate::memory::load_entries(memory_root)
            .into_iter()
            .find(|e| &e.title == title)
        {
            let args = serde_json::json!({ "action": "retire", "title": title });
            if policy.approve_action("evolve_write", &args).await {
                match std::fs::remove_file(&entry.source) {
                    Ok(()) => out.retired.push(title.clone()),
                    Err(e) => out.skipped.push(format!("{title}: delete failed: {e}")),
                }
            } else {
                out.rejected.push(format!("retire {title}: denied"));
            }
        } else {
            out.skipped.push(format!("retire {title}: no such entry"));
        }
    }

    // 2. Insight adds (cap-checked against a fresh disk scan).
    for ins in &proposal.insights {
        let title = ins.title.trim();
        let file_title = title_slug(title);
        if title.is_empty() || file_title.is_empty() {
            out.skipped.push(format!(
                "{title}: unusable title (empty or no file-safe characters)"
            ));
            continue;
        }
        let existing: Vec<String> = crate::memory::load_entries(memory_root)
            .into_iter()
            .map(|e| e.title)
            .collect();
        // The on-disk name is the slug: comparing raw titles alone would let
        // "Rust Cyclic Dep" overwrite an existing "rust-cyclic-dep" entry
        // (or its case variant on a case-insensitive filesystem).
        if existing
            .iter()
            .any(|t| t == title || title_slug(t) == file_title)
            || proposal.duplicates.iter().any(|d| d == title)
        {
            out.skipped.push(format!("{title}: duplicate"));
            continue;
        }
        if existing.len() >= crate::memory::MAX_ENTRIES {
            out.rejected.push(format!(
                "{title}: library full ({} entries)",
                existing.len()
            ));
            continue;
        }
        let args = serde_json::json!({
            "action": "add insight", "title": title, "tags": ins.tags, "content": ins.content,
        });
        if !policy.approve_action("evolve_write", &args).await {
            out.rejected.push(format!("{title}: denied"));
            continue;
        }
        let _ = std::fs::create_dir_all(memory_root);
        let body = bound(&ins.content, 2_000);
        let path = memory_root.join(format!("{file_title}.md"));
        match std::fs::write(
            &path,
            crate::memory::entry_markdown(title, &ins.tags, &now, source_session, &body),
        ) {
            Ok(()) => out.added_insights.push(title.to_string()),
            Err(e) => out.skipped.push(format!("{title}: write failed: {e}")),
        }
    }

    // 3. Skill adds/updates (no cap — the skills dir is user-shared).
    let mut seen_skill_slugs = std::collections::HashSet::new();
    for sk in &proposal.skills {
        let name = slug(&sk.name);
        if name.is_empty() {
            out.skipped.push("skill with empty name".into());
            continue;
        }
        // Same-run collisions must not masquerade as "update skill" — the
        // second proposal with an equal slug would clobber the first while
        // the outcome reports both an add and an update.
        if !seen_skill_slugs.insert(name.clone()) {
            out.skipped
                .push(format!("{name}: duplicate slug in proposal"));
            continue;
        }
        let path = skills_root.join(format!("{name}.md"));
        let _ = std::fs::create_dir_all(skills_root);
        let body = format!(
            "---\nname: {name}\ndescription: {}\nprovenance: evolve\nsource_session: {source_session}\n---\n{}\n",
            sk.description.replace('\n', " "),
            sk.body.trim()
        );
        let updating = path.exists();
        if updating {
            let current = std::fs::read_to_string(&path).unwrap_or_default();
            if !current.contains("provenance: evolve") {
                out.skipped
                    .push(format!("{name}: user-authored, not overwritten"));
                continue;
            }
        }
        let args = serde_json::json!({
            "action": if updating { "update skill" } else { "add skill" },
            "name": name, "description": sk.description, "body": sk.body,
        });
        if !policy.approve_action("evolve_write", &args).await {
            out.rejected.push(format!("{name}: denied"));
            continue;
        }
        match std::fs::write(&path, body) {
            Ok(()) => {
                if updating {
                    out.updated_skills.push(name);
                } else {
                    out.added_skills.push(name);
                }
            }
            Err(e) => out.skipped.push(format!("{name}: write failed: {e}")),
        }
    }

    // 4. Best-effort reindex of the `memory` source (conga-rag feature).
    #[cfg(feature = "rag")]
    index_memory_fail_soft(memory_root).await;
    out
}

/// Fail-soft refresh of the built-in `memory` source after evolve writes.
/// Files are the source of truth — an index lag self-heals on the next
/// `conga-rag ingest` or `rag_remember`. Skipped silently when RAG is
/// unconfigured or the store was never built: evolve must not bootstrap a
/// vector store as a side effect.
#[cfg(feature = "rag")]
async fn index_memory_fail_soft(memory_root: &Path) {
    let Ok((_p, cfg)) = conga_rag::config::RagConfig::load() else {
        return;
    };
    if !memory_root.is_dir() || !cfg.store_path().exists() {
        return;
    }
    match conga_rag::pipeline::run_ingest(&cfg, Some("memory"), false).await {
        Ok(stats) => tracing::info!(
            added = stats.added,
            updated = stats.updated,
            "evolve: memory 源已刷新"
        ),
        Err(e) => tracing::warn!("evolve: memory 索引刷新失败(下次 ingest 补偿): {e}"),
    }
}

/// Default extraction-input budget (chars of rendered transcript).
const MAX_TRAJECTORY_CHARS: usize = 48_000;

/// The whole write path: load trajectory → extract (sub-agent) → parse →
/// admit. `session_id = None` means the current session. Roots and cwd are
/// injected so tests run hermetically against tempdirs; [`Host::evolve`]
/// passes production paths.
pub async fn run_evolve(
    session: &SessionManager,
    policy: &PermissionPolicy,
    spawner: Option<&Arc<dyn SubagentSpawner>>,
    session_id: Option<&str>,
    memory_root: &Path,
    skills_root: &Path,
    cwd: &Path,
) -> Result<EvolveOutcome, conga::AgentError> {
    let sid = match session_id {
        Some(id) => id.to_string(),
        None => session.current_id(),
    };
    let events = session.open_or_migrate(&sid).await?;
    let messages = conga::derive_messages(&events);
    let trajectory = render_trajectory(&messages, MAX_TRAJECTORY_CHARS);
    let catalog = catalog_snapshot(memory_root, cwd, &conga::storage::config_dir());

    let spawner = spawner
        .ok_or_else(|| conga::AgentError::Tool("no subagent spawner wired on this host".into()))?;
    let results = spawner
        .spawn(vec![SubagentSpawn {
            task: extraction_task_prompt(&trajectory, &catalog),
        }])
        .await;
    let result = results
        .into_iter()
        .next()
        .ok_or_else(|| conga::AgentError::Tool("extractor returned no result".into()))?;
    if let Some(err) = result.error {
        return Err(conga::AgentError::Tool(format!("extractor failed: {err}")));
    }
    let proposal = parse_proposal(&result.output)?;
    Ok(apply_proposals(&proposal, memory_root, skills_root, &sid, policy).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use conga::types::message::{FunctionCall, ToolCall};
    use conga::{AgentMessage, ContentBlock, ToolResultMessage};

    #[test]
    fn renders_roles_and_tool_calls() {
        let msgs = vec![
            AgentMessage::user("fix the build"),
            AgentMessage::Assistant(conga::AssistantMessage {
                content: vec![
                    ContentBlock::text("trying a rebuild"),
                    ContentBlock::ToolCall {
                        tool_call: ToolCall {
                            id: "t1".into(),
                            function: FunctionCall {
                                name: "bash".into(),
                                arguments: r#"{"command":"cargo build"}"#.into(),
                            },
                        },
                    },
                ],
                model: "m".into(),
                stop_reason: conga::StopReason::ToolUse,
                usage: None,
                timestamp: 0,
                stream_indices: Vec::new(),
            }),
            AgentMessage::ToolResult(ToolResultMessage {
                tool_call_id: "t1".into(),
                tool_name: "bash".into(),
                content: vec![ContentBlock::text("error: cyclic dependency")],
                is_error: false,
                timestamp: 0,
            }),
        ];
        let out = render_trajectory(&msgs, 10_000);
        assert!(out.contains("## USER\nfix the build"));
        assert!(out.contains("## ASSISTANT\ntrying a rebuild"));
        assert!(out.contains("- tool call: bash("));
        assert!(out.contains("## TOOL RESULT (bash)\nerror: cyclic dependency"));
    }

    #[test]
    fn truncates_oldest_first_and_flags() {
        let mut msgs = Vec::new();
        for i in 0..100 {
            msgs.push(AgentMessage::user(format!(
                "message number {i} with padding padding padding"
            )));
        }
        let out = render_trajectory(&msgs, 2_000);
        assert!(out.starts_with("(older messages truncated"));
        assert!(!out.contains("message number 0"));
        assert!(out.contains("message number 99"));
    }

    #[test]
    fn parses_proposal_with_surrounding_prose() {
        let raw = "Here you go:\n```json\n{\"insights\":[{\"title\":\"t\",\"tags\":[\"a\"],\"content\":\"c\"}],\"skills\":[],\"retires\":[\"old\"],\"duplicates\":[]}\n```\nthanks";
        let p = parse_proposal(raw).unwrap();
        assert_eq!(p.insights.len(), 1);
        assert_eq!(p.insights[0].title, "t");
        assert_eq!(p.retires, vec!["old".to_string()]);
    }

    #[test]
    fn missing_keys_default_to_empty() {
        let p = parse_proposal("{}").unwrap();
        assert!(p.insights.is_empty() && p.skills.is_empty());
    }

    #[test]
    fn garbage_fails_loud() {
        assert!(parse_proposal("no json at all").is_err());
    }

    #[test]
    fn parses_no_panic_on_stray_closing_brace() {
        assert!(parse_proposal("here } take this {\"insights\": [{\"title\":\"t\"").is_err());
    }

    #[test]
    fn extraction_prompt_carries_quality_contract() {
        let p = extraction_task_prompt("TRAJ", "CATALOG");
        assert!(p.contains("root cause"));
        assert!(p.contains("evidence"));
        assert!(p.contains("CATALOG"));
        assert!(p.contains("TRAJ"));
        assert!(p.contains("ONLY a JSON object"));
    }

    use std::sync::Arc;

    fn policy_always(allow: bool) -> crate::permission::PermissionPolicy {
        crate::permission::PermissionPolicy::new(
            crate::permission::Mode::FullAuto,
            Arc::new(move |_name, _args| Box::pin(async move { allow })),
        )
    }

    fn proposal_1() -> EvolveProposal {
        serde_json::from_str(
            r#"{"insights":[{"title":"rust-cyclic-dep","tags":["rust"],"content":"check members first"}],
                "skills":[{"name":"Demo Skill","description":"demo","body":"steps"}],
                "retires":[],"duplicates":[]}"#,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn adds_insight_and_skill_when_approved() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = tmp.path().join("memory");
        let skills = tmp.path().join("skills");
        std::fs::create_dir_all(&mem).unwrap();
        std::fs::create_dir_all(&skills).unwrap();
        let out =
            apply_proposals(&proposal_1(), &mem, &skills, "sess-1", &policy_always(true)).await;
        assert_eq!(out.added_insights, vec!["rust-cyclic-dep".to_string()]);
        assert_eq!(out.added_skills.len(), 1);
        let md = std::fs::read_to_string(skills.join("demo-skill.md")).unwrap();
        assert!(md.contains("provenance: evolve"));
        assert!(md.contains("source_session: sess-1"));
        let entry = std::fs::read_to_string(mem.join("rust-cyclic-dep.md")).unwrap();
        assert!(entry.contains("title: rust-cyclic-dep"));
    }

    #[tokio::test]
    async fn rejected_approval_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = tmp.path().join("memory");
        let skills = tmp.path().join("skills");
        std::fs::create_dir_all(&mem).unwrap();
        std::fs::create_dir_all(&skills).unwrap();
        let out = apply_proposals(&proposal_1(), &mem, &skills, "s", &policy_always(false)).await;
        // 1 insight + 1 skill, retires empty: exactly 2 candidates denied.
        // (Brief said 3, but proposal_1 only carries 2 approvable candidates.)
        assert_eq!(out.rejected.len(), 2);
        assert!(load_test(&mem).is_empty());
        assert!(std::fs::read_dir(&skills).unwrap().count() == 0);
    }

    #[tokio::test]
    async fn cap_rejects_adds_until_retire_frees_slot() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = tmp.path().join("memory");
        std::fs::create_dir_all(&mem).unwrap();
        for i in 0..crate::memory::MAX_ENTRIES {
            std::fs::write(
                mem.join(format!("e{i:02}.md")),
                format!("---\ntitle: e{i:02}\ntags: [t]\ncreated: 1\nsource_session: s\n---\nB.\n"),
            )
            .unwrap();
        }
        let full: EvolveProposal = serde_json::from_str(
            r#"{"insights":[{"title":"new-one","tags":["x"],"content":"c"}],"skills":[],"retires":[],"duplicates":[]}"#,
        )
        .unwrap();
        let out = apply_proposals(
            &full,
            &mem,
            &tmp.path().join("skills"),
            "s",
            &policy_always(true),
        )
        .await;
        assert!(out.rejected.iter().any(|r| r.contains("library full")));

        let with_retire: EvolveProposal = serde_json::from_str(
            r#"{"insights":[{"title":"new-two","tags":["x"],"content":"c"}],"skills":[],"retires":["e00"],"duplicates":[]}"#,
        )
        .unwrap();
        let out = apply_proposals(
            &with_retire,
            &mem,
            &tmp.path().join("skills"),
            "s",
            &policy_always(true),
        )
        .await;
        assert_eq!(out.retired, vec!["e00".to_string()]);
        assert_eq!(out.added_insights, vec!["new-two".to_string()]);
    }

    #[tokio::test]
    async fn user_authored_skill_never_overwritten() {
        let tmp = tempfile::tempdir().unwrap();
        let skills = tmp.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        std::fs::write(
            skills.join("demo-skill.md"),
            "---\nname: demo-skill\ndescription: mine\n---\nHand-written.\n",
        )
        .unwrap();
        let out = apply_proposals(
            &proposal_1(),
            &tmp.path().join("memory"),
            &skills,
            "s",
            &policy_always(true),
        )
        .await;
        assert!(out.skipped.iter().any(|s| s.contains("user-authored")));
        assert!(std::fs::read_to_string(skills.join("demo-skill.md"))
            .unwrap()
            .contains("Hand-written."));
    }

    /// Review fix 1: a title with no ASCII slug (CJK-only) must be skipped,
    /// not written to a hidden `​.md` file that load_entries then ignores —
    /// which would report an add that is invisible to catalog/cap/dedupe.
    #[tokio::test]
    async fn cjk_only_title_skipped_not_written() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = tmp.path().join("memory");
        std::fs::create_dir_all(&mem).unwrap();
        let p: EvolveProposal = serde_json::from_str(
            r#"{"insights":[{"title":"循环依赖排查","tags":["rust"],"content":"c"}],"skills":[],"retires":[],"duplicates":[]}"#,
        )
        .unwrap();
        let out = apply_proposals(
            &p,
            &mem,
            &tmp.path().join("skills"),
            "s",
            &policy_always(true),
        )
        .await;
        assert!(out.skipped.iter().any(|s| s.contains("循环依赖排查")));
        assert!(out.added_insights.is_empty());
        assert_eq!(std::fs::read_dir(&mem).unwrap().count(), 0);
    }

    /// Review fix 2: dedupe must compare slugs, not raw titles — the on-disk
    /// name IS the slug, so "Rust Cyclic Dep" would silently overwrite an
    /// existing "rust-cyclic-dep" entry (or its case variant on APFS).
    #[tokio::test]
    async fn slug_equivalent_title_counts_as_duplicate() {
        let tmp = tempfile::tempdir().unwrap();
        let mem = tmp.path().join("memory");
        std::fs::create_dir_all(&mem).unwrap();
        std::fs::write(
            mem.join("rust-cyclic-dep.md"),
            crate::memory::entry_markdown(
                "rust-cyclic-dep",
                &["rust".to_string()],
                "1",
                "s0",
                "original body",
            ),
        )
        .unwrap();
        let p: EvolveProposal = serde_json::from_str(
            r#"{"insights":[{"title":"Rust Cyclic Dep","tags":["rust"],"content":"new body"}],"skills":[],"retires":[],"duplicates":[]}"#,
        )
        .unwrap();
        let out = apply_proposals(
            &p,
            &mem,
            &tmp.path().join("skills"),
            "s",
            &policy_always(true),
        )
        .await;
        assert!(out.skipped.iter().any(|s| s.contains("duplicate")));
        assert!(out.added_insights.is_empty());
        assert!(std::fs::read_to_string(mem.join("rust-cyclic-dep.md"))
            .unwrap()
            .contains("original body"));
    }

    /// Review fix 3: two proposed skills with equal slugs — the second must
    /// be skipped, not treated as a pseudo-"update" that clobbers the first
    /// while the outcome reports both added and updated.
    #[tokio::test]
    async fn skill_duplicate_slug_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let skills = tmp.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        let p: EvolveProposal = serde_json::from_str(
            r#"{"insights":[],"skills":[{"name":"Demo Skill","description":"d","body":"first body"},{"name":"demo skill","description":"d","body":"second body"}],"retires":[],"duplicates":[]}"#,
        )
        .unwrap();
        let out = apply_proposals(
            &p,
            &tmp.path().join("memory"),
            &skills,
            "s",
            &policy_always(true),
        )
        .await;
        assert_eq!(out.added_skills, vec!["demo-skill".to_string()]);
        assert!(out.updated_skills.is_empty());
        assert!(out
            .skipped
            .iter()
            .any(|s| s.contains("duplicate slug in proposal")));
        assert!(std::fs::read_to_string(skills.join("demo-skill.md"))
            .unwrap()
            .contains("first body"));
    }

    use crate::subagent_types::{SubagentResult, SubagentSpawn, SubagentSpawner};
    use std::future::Future;
    use std::pin::Pin;

    /// Fixed-output extractor: one insight, nothing else. Proves the write
    /// path end-to-end (events -> trajectory -> prompt -> parse -> admit)
    /// without a provider.
    struct FakeExtractor(pub String);
    impl SubagentSpawner for FakeExtractor {
        fn spawn(
            &self,
            _tasks: Vec<SubagentSpawn>,
        ) -> Pin<Box<dyn Future<Output = Vec<SubagentResult>> + Send>> {
            let output = self.0.clone();
            Box::pin(async move {
                vec![SubagentResult {
                    id: "x".into(),
                    task: String::new(),
                    index: 1,
                    summary: String::new(),
                    output,
                    tool_count: 0,
                    error: None,
                    log_path: None,
                }]
            })
        }
    }

    #[tokio::test]
    async fn run_evolve_end_to_end_with_fake_extractor() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let session = crate::SessionManager::with_root(root.clone());
        // Seed a "stumble -> correct" trajectory on the current session.
        session
            .append_event(&conga::SessionEvent::TurnStart)
            .await
            .unwrap();
        session
            .append_event(&conga::SessionEvent::User(conga::AgentMessage::user(
                "fix cyclic dep",
            )))
            .await
            .unwrap();
        let assistant = conga::AgentMessage::assistant_text(
            "Root cause: conga-ext path-ref'd conga-host. Removed the edge; build green.",
        );
        session
            .append_event(&conga::SessionEvent::from_message(&assistant, None).unwrap())
            .await
            .unwrap();

        let spawner: Arc<dyn SubagentSpawner> = Arc::new(FakeExtractor(
            r#"{"insights":[{"title":"rust-cyclic-dep","tags":["rust","cargo"],"content":"check members first"}],
                "skills":[],"retires":[],"duplicates":[]}"#
                .to_string(),
        ));
        let mem = root.join("memory");
        let skills = root.join("skills");
        let out = run_evolve(
            &session,
            &policy_always(true),
            Some(&spawner),
            None,
            &mem,
            &skills,
            &root,
        )
        .await
        .unwrap();
        assert_eq!(out.added_insights, vec!["rust-cyclic-dep".to_string()]);
        assert!(mem.join("rust-cyclic-dep.md").exists());
        // Source session provenance rides along.
        let entry = std::fs::read_to_string(mem.join("rust-cyclic-dep.md")).unwrap();
        assert!(entry.contains(&format!("source_session: {}", session.current_id())));
    }

    fn load_test(root: &Path) -> Vec<crate::memory::MemoryEntry> {
        crate::memory::load_entries(root)
    }

    // --- feature rag: memory source reindex ---

    /// CONGA_RAG_CONFIG / CONGA_RAG_BUILTIN_BASE are process-global; the
    /// rag-feature tests serialize on this lock (async variant of the
    /// conga-ext ENV_LOCK pattern).
    #[cfg(feature = "rag")]
    async fn rag_env_lock() -> tokio::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await
    }

    #[cfg(feature = "rag")]
    fn mini_rag_config(
        base: &std::path::Path,
        store: &std::path::Path,
        emb: &str,
    ) -> std::path::PathBuf {
        let dummy = base.join("dummy");
        std::fs::create_dir_all(&dummy).unwrap();
        std::fs::write(dummy.join("seed.md"), "seed content").unwrap();
        let p = base.join("rag.toml");
        std::fs::write(
            &p,
            format!(
                r#"[sources.dummy]
type = "dir"
path = {:?}

[embedding]
base_url = {:?}
api_key = "k"
model = "mock"

[store]
path = {:?}
"#,
                dummy, emb, store
            ),
        )
        .unwrap();
        p
    }

    #[cfg(feature = "rag")]
    fn one_insight_proposal() -> EvolveProposal {
        serde_json::from_str(
            r#"{"insights":[{"title":"rust-cyclic-dep","tags":["rust"],"content":"check members first"}]}"#,
        )
        .unwrap()
    }

    #[tokio::test]
    #[cfg(feature = "rag")]
    async fn evolve_writes_then_reindexes_memory_source() {
        let _g = rag_env_lock().await;
        let base = tempfile::tempdir().unwrap();
        let memory_root = base.path().join("memory");
        std::fs::create_dir_all(&memory_root).unwrap();
        std::fs::create_dir_all(base.path().join("notes")).unwrap(); // 空目录,0 文件
        let (emb, _req) = conga_rag::testsupport::spawn_mock_embeddings(0).await;
        let store_path = base.path().join("t.db");
        let cfg_path = mini_rag_config(base.path(), &store_path, &emb);
        std::env::set_var("CONGA_RAG_CONFIG", &cfg_path);
        std::env::set_var("CONGA_RAG_BUILTIN_BASE", base.path());
        // 预建 store(hook 对不存在的 store 只跳过,不引导)
        {
            let (_p, cfg) = conga_rag::config::RagConfig::load().unwrap();
            conga_rag::pipeline::run_ingest(&cfg, None, false)
                .await
                .unwrap();
        }

        let prop = one_insight_proposal();
        let out = apply_proposals(
            &prop,
            &memory_root,
            &base.path().join("skills"),
            "s1",
            &policy_always(true),
        )
        .await;
        assert_eq!(out.added_insights.len(), 1);

        let (_p, cfg) = conga_rag::config::RagConfig::load().unwrap();
        let store = conga_rag::store::Store::open(&cfg.store_path())
            .await
            .unwrap();
        let docs = store.docs_for_source("memory").await.unwrap();
        store.close().await.unwrap();
        std::env::remove_var("CONGA_RAG_CONFIG");
        std::env::remove_var("CONGA_RAG_BUILTIN_BASE");
        assert!(!docs.is_empty(), "蒸馏产物已入索引: {docs:?}");
    }

    #[tokio::test]
    #[cfg(feature = "rag")]
    async fn evolve_survives_index_failure() {
        let _g = rag_env_lock().await;
        // rag 配置指向不存在的路径 → hook 静默跳过,evolve 照常成功
        std::env::set_var("CONGA_RAG_CONFIG", "/nonexistent/rag.toml");
        std::env::set_var("CONGA_RAG_BUILTIN_BASE", "");
        let base = tempfile::tempdir().unwrap();
        let out = apply_proposals(
            &one_insight_proposal(),
            &base.path().join("memory"),
            &base.path().join("skills"),
            "s1",
            &policy_always(true),
        )
        .await;
        std::env::remove_var("CONGA_RAG_CONFIG");
        std::env::remove_var("CONGA_RAG_BUILTIN_BASE");
        assert_eq!(
            out.added_insights.len(),
            1,
            "fail-soft:索引失败不影响 evolve"
        );
    }
}
