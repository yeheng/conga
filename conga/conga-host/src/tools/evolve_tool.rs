//! `evolve` tool — transport-agnostic entry to self-evolution. The CLI
//! calls `Host::evolve` directly (no main-model dispatch round trip);
//! gateway/desktop reach the same `run_evolve` core through this tool.
//! Risk = High: the permission matrix gates it (blocked in Suggest/Plan,
//! approved per-call in AutoEdit), same as every other mutating action.

use std::sync::Arc;

use crate::permission::PermissionPolicy;
use crate::session::SessionManager;
use crate::subagent_types::SubagentSpawner;
use conga::types::tool::{RiskLevel, ToolCallCtx, ToolDefinition, ToolResult};

/// Live state the tool needs. Built by `Host::with_spawner`; `None` (the
/// `built_in_tools()` default) means this host never wired a spawner and
/// the tool reports unavailability instead of half-running.
pub struct EvolveHandle {
    pub session: SessionManager,
    pub policy: Arc<PermissionPolicy>,
    pub spawner: Option<Arc<dyn SubagentSpawner>>,
}

pub fn tool(handle: Option<EvolveHandle>) -> ToolDefinition {
    let handle = handle.map(Arc::new);
    ToolDefinition {
        name: "evolve".into(),
        label: "Evolve".into(),
        description: "Distill the current session (or a given one) into \
reusable memory insights and skills. Every write is individually approved \
by the user. Only call this when the user asks to evolve/learn/distill."
            .into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "Optional session id to distill; default: the current session"
                }
            }
        }),
        risk: RiskLevel::High,
        execute: Arc::new(move |ctx: ToolCallCtx| {
            let handle = handle.clone();
            Box::pin(async move {
                if ctx.aborted() {
                    return Ok(ToolResult::error("aborted".to_string()));
                }
                let Some(h) = handle else {
                    return Ok(ToolResult::error(
                        "evolve unavailable: no spawner wired on this host".to_string(),
                    ));
                };
                let sid = ctx
                    .args
                    .get("session_id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let config = conga::storage::config_dir();
                match crate::evolve::run_evolve(
                    &h.session,
                    &h.policy,
                    h.spawner.as_ref(),
                    sid.as_deref(),
                    &config.join("memory"),
                    &config.join("skills"),
                    &ctx.ctx.cwd,
                )
                .await
                {
                    Ok(out) => Ok(ToolResult::text(format!(
                        "{}\n{}",
                        out.summarize(),
                        details(&out)
                    ))),
                    Err(e) => Ok(ToolResult::error(format!("evolve failed: {e}"))),
                }
            })
        }),
    }
}

/// Per-item lines under the one-line summary: `+` added, `~` updated,
/// `-` retired, `!` rejected/skipped (the human already saw each one in
/// its approval prompt — this is the audit trail, not new information).
fn details(out: &crate::evolve::EvolveOutcome) -> String {
    let mut lines = Vec::new();
    for t in out.added_insights.iter().chain(&out.added_skills) {
        lines.push(format!("+ {t}"));
    }
    for t in &out.updated_skills {
        lines.push(format!("~ {t}"));
    }
    for t in &out.retired {
        lines.push(format!("- {t}"));
    }
    for t in out.rejected.iter().chain(&out.skipped) {
        lines.push(format!("! {t}"));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Built without a handle (`built_in_tools()` path, or a host that never
    /// called `with_spawner`): the tool must name itself `evolve`, grade
    /// itself High risk, and report unavailability as a flagged error rather
    /// than crashing or pretending to succeed.
    #[tokio::test]
    async fn no_handle_reports_unavailable() {
        let t = tool(None);
        assert_eq!(t.name, "evolve");
        assert_eq!(t.risk, conga::types::tool::RiskLevel::High);
        let ctx = conga::types::tool::ToolCallCtx {
            tool_call_id: "tc".into(),
            args: serde_json::json!({}),
            signal: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            ctx: conga::types::tool::ToolContext {
                cwd: std::path::PathBuf::from("."),
                env: Default::default(),
                session_id: "s".into(),
                state_dir: std::path::PathBuf::from("."),
            },
        };
        let res = (t.execute)(ctx).await.unwrap();
        // ToolResult carries text as content blocks and has no `output`
        // field; the unavailable path is `ToolResult::error` → `is_error`.
        assert!(res.is_error);
        let text = res
            .content
            .iter()
            .filter_map(|b| match b {
                conga::ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert!(text.contains("unavailable"));
    }

    /// The audit trail under the one-line summary: every outcome class gets
    /// its own marker, in a fixed order.
    #[test]
    fn details_lists_every_outcome_class_with_its_marker() {
        let out = crate::evolve::EvolveOutcome {
            added_insights: vec!["insight-a".into()],
            added_skills: vec!["skill-a".into()],
            updated_skills: vec!["skill-b".into()],
            retired: vec!["skill-c".into()],
            rejected: vec!["insight-b".into()],
            skipped: vec!["insight-c".into()],
        };
        let rendered = details(&out);
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(
            lines,
            vec![
                "+ insight-a",
                "+ skill-a",
                "~ skill-b",
                "- skill-c",
                "! insight-b",
                "! insight-c",
            ]
        );
    }

    /// Boundary: nothing happened -> no detail lines at all (not even a
    /// blank one), so the summary stays clean.
    #[test]
    fn details_is_empty_for_empty_outcome() {
        let out = crate::evolve::EvolveOutcome::default();
        assert_eq!(details(&out), "");
    }

    /// The tool's schema advertises the optional session id.
    #[test]
    fn tool_schema_exposes_optional_session_id() {
        let t = tool(None);
        assert_eq!(t.label, "Evolve");
        assert!(t.parameters.is_object());
        assert!(
            t.parameters["properties"]["session_id"]["type"] == "string",
            "{:?}",
            t.parameters
        );
    }
}
