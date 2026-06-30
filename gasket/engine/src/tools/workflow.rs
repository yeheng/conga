//! Native workflow engine — state-machine driven multi-step agent pipelines.
//!
//! Workflows are YAML-defined directed graphs (with loops for retries) that
//! orchestrate subagent spawning. Each step feeds its output into the next
//! step via simple `{{variable}}` template substitution.
//!
//! Key design decisions:
//! - Stateless: all execution context lives in a `HashMap<String, String>`.
//! - Crash-safe: no external daemon, no IPC, no Python interpreter.
//! - Streaming: each subagent step uses `spawn_with_stream` so the frontend
//!   sees real-time thinking and tool-call events.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Context;
use async_trait::async_trait;
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;
use tracing::{info, warn};

use super::spawn_common::spawn_event_forwarder;
use super::{Tool, ToolContext, ToolError, ToolResult};

// ── Data structures ─────────────────────────────────────────────────────────

/// Execution mode for a workflow.
///
/// `Tool` (default): the workflow is registered as a callable tool and
/// executed as a state machine via subagent spawning.
/// `Skill`: the workflow is injected into the system prompt as a markdown
/// skill and executed autonomously by the LLM.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WorkflowMode {
    Tool,
    Skill,
}

fn default_workflow_mode() -> WorkflowMode {
    WorkflowMode::Tool
}

fn default_always_load() -> bool {
    true
}

/// A workflow manifest loaded from a YAML file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowManifest {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub start_step: String,
    pub steps: HashMap<String, WorkflowStepDef>,
    /// Optional template to render as the final output instead of raw JSON.
    #[serde(default)]
    pub output_template: Option<String>,
    /// Execution mode (see [`WorkflowMode`]). Defaults to `Tool`.
    #[serde(default = "default_workflow_mode")]
    pub mode: WorkflowMode,
    /// Whether this workflow (in skill mode) is eagerly injected into the
    /// system prompt. Ignored in tool mode. Defaults to `true`.
    #[serde(default = "default_always_load")]
    pub always: bool,
}

/// Shared regex for `{{key}}` template placeholders.
///
/// Key matches alphanumeric + dots + underscores + slashes.
static PLACEHOLDER_RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();

fn placeholder_re() -> &'static Regex {
    PLACEHOLDER_RE.get_or_init(|| Regex::new(r"\{\{([a-zA-Z0-9_./]+)\}\}").unwrap())
}

/// Strip all `{{key}}` template placeholders from a prompt, replacing them
/// with the literal text `[see above]` for readability in skill-mode output.
fn clean_template_placeholders(template: &str) -> String {
    placeholder_re()
        .replace_all(template, "[see above]")
        .to_string()
}

/// Step definition as it appears in the YAML manifest.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowStepDef {
    /// Prompt template with `{{key}}` placeholders.
    pub prompt: String,
    /// Optional model override for this step.
    #[serde(default)]
    pub model: Option<String>,
    /// Next step name. Absent when `evaluate` is present.
    #[serde(default)]
    pub next: Option<String>,
    /// Evaluation configuration for verdict-based branching.
    #[serde(default)]
    pub evaluate: Option<EvaluateConfigDef>,
    /// Optional whitelist of tool names visible to the LLM for this step.
    #[serde(default)]
    pub tools: Option<Vec<String>>,
}

/// Evaluation configuration as it appears in the YAML manifest.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluateConfigDef {
    /// Step to go to when evaluation passes.
    pub on_pass: String,
    /// Step to go to when evaluation fails.
    pub on_fail: String,
    /// Maximum retry attempts before giving up.
    #[serde(default = "default_max_retries")]
    pub max_retries: usize,
}

fn default_max_retries() -> usize {
    3
}

// ── Internal execution structures ───────────────────────────────────────────

/// A validated workflow ready for state-machine execution.
/// Steps are indexed by `usize` rather than string names — no runtime lookups.
#[derive(Clone)]
pub struct Workflow {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub steps: Vec<Step>,
    pub start_idx: usize,
    pub output_template: Option<String>,
    /// Whether this workflow (in skill mode) is eagerly loaded into the
    /// system prompt. Tool-mode workflows ignore this field.
    pub always: bool,
}

/// A single step in the validated workflow.
#[derive(Clone)]
pub struct Step {
    pub name: String,
    pub prompt: String,
    pub model: Option<String>,
    pub transition: Transition,
    /// Optional whitelist of tool names visible to the LLM for this step.
    pub tools: Option<Vec<String>>,
}

/// How execution continues after a step completes.
#[derive(Clone)]
pub enum Transition {
    /// Proceed to the step at the given index.
    Next(usize),
    /// Evaluate the step output and branch.
    Evaluate(EvalGate),
    /// Terminate the workflow.
    Done,
}

/// Branching gate for evaluate transitions.
///
/// `None` means terminate the workflow (the YAML `"DONE"` sentinel).
#[derive(Clone)]
pub struct EvalGate {
    pub on_pass: Option<usize>,
    pub on_fail: Option<usize>,
    pub max_retries: usize,
}

impl Workflow {
    /// Validate a manifest and build the indexed execution graph.
    ///
    /// Steps are ordered by the primary execution path (starting from
    /// `start_step` and following `next` / `evaluate.on_pass` chains).
    /// Branches reached only via `evaluate.on_fail` are appended after the
    /// primary path in the order they were first encountered. Steps
    /// unreachable from `start_step` cause an error — no silent dead steps.
    pub fn from_manifest(manifest: &WorkflowManifest) -> anyhow::Result<Self> {
        // Basic sanity checks.
        if manifest.name.is_empty() {
            return Err(anyhow::anyhow!("Workflow has empty name"));
        }
        if manifest.description.is_empty() {
            return Err(anyhow::anyhow!("Workflow has empty description"));
        }
        if manifest.steps.is_empty() {
            return Err(anyhow::anyhow!("Workflow has no steps"));
        }
        if !manifest.steps.contains_key(&manifest.start_step) {
            return Err(anyhow::anyhow!(
                "start_step '{}' not found in steps",
                manifest.start_step
            ));
        }

        // Build deterministic ordering: walk primary path (next / on_pass)
        // first, queueing on_fail targets for traversal after the primary
        // path is exhausted.
        let mut ordered = Vec::with_capacity(manifest.steps.len());
        let mut visited = std::collections::HashSet::new();
        let mut secondary: std::collections::VecDeque<String> = std::collections::VecDeque::new();
        let mut current = manifest.start_step.clone();

        loop {
            while current != "DONE" && !visited.contains(&current) {
                let Some(step) = manifest.steps.get(&current) else {
                    break;
                };
                visited.insert(current.clone());
                ordered.push(current.clone());
                if let Some(ref eval) = step.evaluate {
                    if eval.on_fail != "DONE" {
                        secondary.push_back(eval.on_fail.clone());
                    }
                    current = eval.on_pass.clone();
                } else if let Some(ref next) = step.next {
                    current = next.clone();
                } else {
                    break;
                }
            }
            match secondary.pop_front() {
                Some(name) => current = name,
                None => break,
            }
        }

        // Reject unreachable steps. A defined-but-unreachable step is almost
        // always a YAML typo or refactoring leftover; fail loudly.
        let mut unreachable: Vec<String> = manifest
            .steps
            .keys()
            .filter(|n| !visited.contains(*n))
            .cloned()
            .collect();
        if !unreachable.is_empty() {
            unreachable.sort();
            return Err(anyhow::anyhow!(
                "Workflow has unreachable steps: {:?}",
                unreachable
            ));
        }

        let name_to_idx: HashMap<String, usize> = ordered
            .iter()
            .enumerate()
            .map(|(i, n)| (n.clone(), i))
            .collect();
        let start_idx = *name_to_idx
            .get(&manifest.start_step)
            .expect("invariant: start_step is the first entry in `ordered`");

        let mut steps = Vec::with_capacity(ordered.len());
        for name in &ordered {
            let def = manifest
                .steps
                .get(name)
                .expect("invariant: `ordered` only contains names from `manifest.steps`");
            let transition = if let Some(ref eval_def) = def.evaluate {
                let on_pass = if eval_def.on_pass == "DONE" {
                    None
                } else {
                    Some(*name_to_idx.get(&eval_def.on_pass).ok_or_else(|| {
                        anyhow::anyhow!(
                            "step '{}' evaluate.on_pass '{}' not found",
                            name,
                            eval_def.on_pass
                        )
                    })?)
                };
                let on_fail = if eval_def.on_fail == "DONE" {
                    None
                } else {
                    Some(*name_to_idx.get(&eval_def.on_fail).ok_or_else(|| {
                        anyhow::anyhow!(
                            "step '{}' evaluate.on_fail '{}' not found",
                            name,
                            eval_def.on_fail
                        )
                    })?)
                };
                Transition::Evaluate(EvalGate {
                    on_pass,
                    on_fail,
                    max_retries: eval_def.max_retries,
                })
            } else if let Some(ref next) = def.next {
                if next == "DONE" {
                    Transition::Done
                } else {
                    let next_idx = *name_to_idx.get(next).ok_or_else(|| {
                        anyhow::anyhow!("step '{}' next '{}' not found", name, next)
                    })?;
                    Transition::Next(next_idx)
                }
            } else {
                return Err(anyhow::anyhow!(
                    "step '{}' has neither 'next' nor 'evaluate'",
                    name
                ));
            };

            steps.push(Step {
                name: name.clone(),
                prompt: def.prompt.clone(),
                model: def.model.clone(),
                transition,
                tools: def.tools.clone(),
            });
        }

        Ok(Self {
            name: manifest.name.clone(),
            description: manifest.description.clone(),
            parameters: manifest.parameters.clone(),
            steps,
            start_idx,
            output_template: manifest.output_template.clone(),
            always: manifest.always,
        })
    }

    /// Convert this workflow into markdown skill content for system prompt injection.
    ///
    /// In skill mode the LLM executes steps autonomously through the normal
    /// agent loop; no state machine or subagent spawning is used.
    pub fn to_skill_content(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!("## Workflow: {}\n\n", self.name));
        out.push_str(&self.description);
        out.push('\n');

        // Parameters guidance
        if let Some(props) = self
            .parameters
            .get("properties")
            .and_then(|v| v.as_object())
        {
            if !props.is_empty() {
                out.push_str("\n**Parameters**:\n");
                for (k, v) in props {
                    let desc = v.get("description").and_then(|d| d.as_str()).unwrap_or("");
                    let default = v.get("default").map(|d| d.to_string());
                    out.push_str(&format!("- `{}`", k));
                    if !desc.is_empty() {
                        out.push_str(&format!(": {}", desc));
                    }
                    if let Some(d) = default {
                        out.push_str(&format!(" (default: {})", d));
                    }
                    out.push('\n');
                }
            }
        }

        out.push_str("\n**Execution Rules**:\n");
        out.push_str(
            "1. Execute the following steps in order. Confirm completion before proceeding.\n",
        );
        out.push_str("2. Context flows naturally through conversation history; explicit step numbers are not required.\n");
        out.push_str(
            "3. If a step is clearly unnecessary, skip it flexibly but inform the user.\n",
        );

        out.push_str("\n### Execution Steps\n");

        // Traverse primary path (same logic as execution, but only for display).
        let mut visited = std::collections::HashSet::new();
        let mut current = self.start_idx;
        let mut step_num = 1;

        while !visited.contains(&current) {
            visited.insert(current);
            let step = &self.steps[current];

            out.push('\n');
            out.push_str(&format!("#### {}. {}\n", step_num, step.name));
            let cleaned = clean_template_placeholders(&step.prompt);
            out.push_str(&cleaned);
            out.push('\n');

            if let Transition::Evaluate(gate) = &step.transition {
                let pass_name = gate
                    .on_pass
                    .map(|idx| self.steps[idx].name.as_str())
                    .unwrap_or("DONE");
                let fail_name = gate
                    .on_fail
                    .map(|idx| self.steps[idx].name.as_str())
                    .unwrap_or("DONE");
                out.push_str("\n*Review Rules*:\n");
                out.push_str(&format!("- On pass proceed to: **{}**\n", pass_name));
                out.push_str(&format!(
                    "- On fail return to: **{}** (max retries: {})\n",
                    fail_name, gate.max_retries
                ));
            }

            match &step.transition {
                Transition::Next(idx) => current = *idx,
                Transition::Evaluate(gate) => match gate.on_pass {
                    Some(idx) => current = idx,
                    None => break,
                },
                Transition::Done => break,
            }
            step_num += 1;
        }

        out.push('\n');
        out.push_str("**End Rule**: After all steps are complete, output the final result and explicitly inform the user that the workflow has finished.\n");

        out
    }
}

// ── Template substitution ───────────────────────────────────────────────────

/// Substitute `{{key}}` placeholders in a template using values from `ctx`.
///
/// Unknown keys are left as-is so that missing variables are obvious in the
/// LLM prompt rather than silently replaced with empty strings.
fn substitute_template(template: &str, ctx: &HashMap<String, String>) -> String {
    placeholder_re()
        .replace_all(template, |caps: &regex::Captures| {
            let key = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            ctx.get(key)
                .cloned()
                .unwrap_or_else(|| caps.get(0).unwrap().as_str().to_owned())
        })
        .to_string()
}

// ── Verdict parsing ─────────────────────────────────────────────────────────

/// Extract the first complete JSON object from arbitrary LLM output.
///
/// Tolerates surrounding prose, markdown fences, and trailing commentary by
/// locating the first `{` and using `serde_json`'s streaming deserializer to
/// stop at the matching `}`. This is the right level of robustness for LLM
/// output: strict prompting can't always prevent trailing explanations.
fn extract_first_json_object(text: &str) -> Result<Value, String> {
    let trimmed = text.trim();
    let start = trimmed
        .find('{')
        .ok_or_else(|| "No JSON object found in output".to_string())?;
    let slice = &trimmed[start..];
    serde_json::Deserializer::from_str(slice)
        .into_iter::<Value>()
        .next()
        .ok_or_else(|| "No JSON object found in output".to_string())?
        .map_err(|e| format!("JSON parse error: {}", e))
}

/// Parse a verdict from LLM output.
///
/// Primary schema: `{"verdict": "PASS"|"FAIL", "reason": "..."}`.
/// For backward compatibility this also accepts the legacy boolean aliases
/// `pass_gate` and `validation_passed` (with a deprecation warning) and
/// synthesizes a `reason` from the object when missing. Unparseable input is
/// an error so the caller can decide to retry.
fn parse_verdict(text: &str) -> Result<(String, String), String> {
    let obj = extract_first_json_object(text)?;

    let verdict = if let Some(s) = obj.get("verdict").and_then(|v| v.as_str()) {
        s.to_uppercase()
    } else if let Some(b) = obj.get("pass_gate").and_then(|v| v.as_bool()) {
        warn!(
            "Workflow verdict using deprecated alias 'pass_gate'; \
             please migrate to {{\"verdict\":\"PASS|FAIL\",\"reason\":...}}"
        );
        if b {
            "PASS".to_string()
        } else {
            "FAIL".to_string()
        }
    } else if let Some(b) = obj.get("validation_passed").and_then(|v| v.as_bool()) {
        warn!(
            "Workflow verdict using deprecated alias 'validation_passed'; \
             please migrate to {{\"verdict\":\"PASS|FAIL\",\"reason\":...}}"
        );
        if b {
            "PASS".to_string()
        } else {
            "FAIL".to_string()
        }
    } else {
        return Err("Missing 'verdict' field".to_string());
    };

    if verdict != "PASS" && verdict != "FAIL" {
        return Err(format!(
            "Invalid verdict '{}', expected PASS or FAIL",
            verdict
        ));
    }

    // `reason` is informational. Prefer the explicit field; otherwise serialize
    // the object so the downstream loop-back step still has something to use.
    let reason = obj
        .get("reason")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| serde_json::to_string(&obj).unwrap_or_default());

    Ok((verdict, reason))
}

// ── WorkflowTool ────────────────────────────────────────────────────────────

/// A tool that executes a YAML-defined workflow as a state machine.
#[derive(Clone)]
pub struct WorkflowTool {
    workflow: Workflow,
}

impl WorkflowTool {
    /// Create a new workflow tool from a validated workflow.
    pub fn new(workflow: Workflow) -> Self {
        Self { workflow }
    }

    /// Execute a single step: spawn subagent, stream events, collect result.
    async fn run_step(
        &self,
        step: &Step,
        prompt: &str,
        step_index: usize,
        ctx: &ToolContext,
    ) -> Result<String, ToolError> {
        let spawner = ctx.spawner.as_ref().ok_or_else(|| {
            ToolError::ExecutionError(
                "Subagent spawning is not available in this context".to_string(),
            )
        })?;

        info!(
            "[Workflow {}] Step '{}' spawning subagent",
            self.workflow.name, step.name
        );

        // Spawn with streaming so the frontend sees real-time progress.
        let (subagent_id, event_rx, result_rx, _cancel_token) = spawner
            .spawn_with_stream(
                prompt.to_string(),
                step.model.clone(),
                ctx,
                step.tools.clone(),
            )
            .await
            .map_err(|e| ToolError::ExecutionError(format!("Failed to spawn subagent: {}", e)))?;

        // Notify frontend that subagent has started.
        let _ = ctx
            .outbound_tx
            .send(gasket_types::events::OutboundMessage::with_ws_message(
                ctx.session_key.channel.clone(),
                ctx.session_key.chat_id.clone(),
                gasket_types::events::ChatEvent::subagent_started(
                    subagent_id.clone(),
                    &step.name,
                    step_index as u32,
                ),
            ))
            .await;

        // Forward streaming events to WebSocket in the background.
        let forward_handle = spawn_event_forwarder(
            subagent_id.clone(),
            event_rx,
            ctx.session_key.clone(),
            ctx.outbound_tx.clone(),
        );

        // Block for the final result.
        let result = result_rx.await.map_err(|e| {
            ToolError::ExecutionError(format!("Subagent result channel closed: {}", e))
        })?;

        // Ensure event forwarding completes (or channel is closed) before returning.
        let _ = forward_handle.await;

        info!(
            "[Workflow {}] Step '{}' completed (tools_used: {})",
            self.workflow.name,
            step.name,
            result.response.tools_used.len()
        );

        // Notify frontend that subagent has completed.
        let _ = ctx
            .outbound_tx
            .send(gasket_types::events::OutboundMessage::with_ws_message(
                ctx.session_key.channel.clone(),
                ctx.session_key.chat_id.clone(),
                gasket_types::events::ChatEvent::subagent_completed(
                    subagent_id,
                    step_index as u32,
                    result.response.content.clone(),
                    result.response.tools_used.len() as u32,
                ),
            ))
            .await;

        Ok(result.response.content)
    }
}

#[async_trait]
impl Tool for WorkflowTool {
    fn name(&self) -> &str {
        &self.workflow.name
    }

    fn description(&self) -> &str {
        &self.workflow.description
    }

    fn parameters(&self) -> Value {
        self.workflow.parameters.clone()
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    #[tracing::instrument(name = "tool.workflow", skip_all, fields(workflow = %self.workflow.name))]
    async fn execute(&self, args: Value, ctx: &ToolContext) -> ToolResult {
        // Flatten user arguments into the context map under the "input." prefix.
        let mut context_map = HashMap::new();
        if let Some(obj) = args.as_object() {
            for (k, v) in obj {
                let val_str = v
                    .as_str()
                    .map(String::from)
                    .unwrap_or_else(|| v.to_string());
                context_map.insert(format!("input.{}", k), val_str);
            }
        }

        let mut current_idx = self.workflow.start_idx;
        let mut retry_counts: HashMap<usize, usize> = HashMap::new();
        let mut step_index = 0usize;
        const MAX_WORKFLOW_STEPS: usize = 100;

        loop {
            if step_index >= MAX_WORKFLOW_STEPS {
                return Err(ToolError::ExecutionError(format!(
                    "Workflow exceeded maximum step limit ({})",
                    MAX_WORKFLOW_STEPS
                )));
            }

            let step = &self.workflow.steps[current_idx];

            // Substitute template variables.
            let prompt = substitute_template(&step.prompt, &context_map);

            // Execute the step via subagent.
            let result = self.run_step(step, &prompt, step_index, ctx).await?;
            step_index += 1;

            // Store result keyed by step name so templates like {{research}} resolve.
            context_map.insert(step.name.clone(), result);

            // Determine next step.
            match &step.transition {
                Transition::Next(idx) => current_idx = *idx,
                Transition::Evaluate(gate) => {
                    let review_text = context_map.get(&step.name).cloned().unwrap_or_default();
                    let (verdict, reason) = parse_verdict(&review_text).unwrap_or_else(|e| {
                        warn!(
                            "[Workflow {}] Verdict parse failed for step '{}': {}",
                            self.workflow.name, step.name, e
                        );
                        ("FAIL".to_string(), e)
                    });

                    // Store reason for template use in the loop-back step.
                    context_map.insert(format!("{}.reason", step.name), reason.clone());

                    if verdict == "PASS" {
                        match gate.on_pass {
                            Some(idx) => current_idx = idx,
                            None => break,
                        }
                    } else {
                        let retries = retry_counts.entry(current_idx).or_insert(0);
                        *retries += 1;
                        if *retries > gate.max_retries {
                            return Err(ToolError::ExecutionError(format!(
                                "Workflow step '{}' failed after {} retries. Last reason: {}",
                                step.name, gate.max_retries, reason
                            )));
                        }
                        match gate.on_fail {
                            Some(idx) => current_idx = idx,
                            None => break,
                        }
                    }
                }
                Transition::Done => break,
            }
        }

        // If an output_template is defined, render it; otherwise return the raw context JSON.
        if let Some(ref template) = self.workflow.output_template {
            Ok(substitute_template(template, &context_map))
        } else {
            let final_output = serde_json::json!({
                "context": context_map,
            });
            serde_json::to_string(&final_output).map_err(|e| {
                ToolError::ExecutionError(format!("Failed to serialize result: {}", e))
            })
        }
    }
}

// ── Discovery ─────────────────────────────────────────────────────────────────

/// Discover and load all workflow manifests in a directory.
///
/// Scans `*.yaml` and `*.yml` files, parses them as `WorkflowManifest`,
/// and wraps each in a `WorkflowTool`.
pub fn discover_workflows(
    workflows_dir: &Path,
) -> anyhow::Result<Vec<WorkflowTool>> {
    let mut tools = Vec::new();

    if !workflows_dir.exists() {
        tracing::info!(
            "Workflows directory does not exist: {:?}, skipping discovery",
            workflows_dir
        );
        return Ok(tools);
    }

    let entries = std::fs::read_dir(workflows_dir)
        .with_context(|| format!("Failed to read workflows directory {:?}", workflows_dir))?;

    for entry in entries {
        let entry = entry
            .with_context(|| format!("Failed to read directory entry in {:?}", workflows_dir))?;

        let path = entry.path();
        if path.is_dir() {
            continue;
        }

        let ext = path.extension().and_then(|s| s.to_str());
        if ext != Some("yaml") && ext != Some("yml") {
            continue;
        }

        match load_workflow(&path) {
            Ok(manifest) => {
                if manifest.mode == WorkflowMode::Skill {
                    tracing::debug!(
                        "Skipping workflow '{}' from tool registry (skill mode)",
                        manifest.name
                    );
                    continue;
                }
                match Workflow::from_manifest(&manifest) {
                    Ok(workflow) => {
                        info!("Discovered workflow '{}' from {:?}", workflow.name, path);
                        tools.push(WorkflowTool::new(workflow));
                    }
                    Err(e) => {
                        warn!("Failed to validate workflow from {:?}: {}", path, e);
                    }
                }
            }
            Err(e) => {
                warn!("Failed to load workflow from {:?}: {}", path, e);
            }
        }
    }

    Ok(tools)
}

/// Load a single workflow manifest from a YAML file.
///
/// Performs only YAML parsing; structural validation happens in
/// `Workflow::from_manifest` when the workflow is prepared for execution.
pub(crate) fn load_workflow(path: &Path) -> anyhow::Result<WorkflowManifest> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("Failed to read workflow file {:?}: {}", path, e))?;

    let manifest: WorkflowManifest = serde_yaml::from_str(&content)
        .map_err(|e| anyhow::anyhow!("Failed to parse workflow YAML from {:?}: {}", path, e))?;

    Ok(manifest)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_substitution_basic() {
        let mut ctx = HashMap::new();
        ctx.insert("input.task".to_string(), "build a cat".to_string());
        ctx.insert("research".to_string(), "cats are furry".to_string());

        let template = "Task: {{input.task}}\nResearch: {{research}}\nMissing: {{missing}}";
        let result = substitute_template(template, &ctx);
        assert_eq!(
            result,
            "Task: build a cat\nResearch: cats are furry\nMissing: {{missing}}"
        );
    }

    #[test]
    fn parse_verdict_clean_json() {
        let text = r#"{"verdict": "PASS", "reason": "looks good"}"#;
        let (v, r) = parse_verdict(text).unwrap();
        assert_eq!(v, "PASS");
        assert_eq!(r, "looks good");
    }

    #[test]
    fn parse_verdict_with_fences() {
        let text = "```json\n{\"verdict\": \"FAIL\", \"reason\": \"bad code\"}\n```";
        let (v, r) = parse_verdict(text).unwrap();
        assert_eq!(v, "FAIL");
        assert_eq!(r, "bad code");
    }

    #[test]
    fn parse_verdict_unparseable_defaults_to_fail() {
        let text = "this is not json at all";
        let result = parse_verdict(text);
        assert!(result.is_err());
    }

    #[test]
    fn parse_verdict_tolerates_trailing_prose() {
        let text = r#"{"verdict": "PASS", "reason": "ok"}

Note: this was a clean run with no issues."#;
        let (v, r) = parse_verdict(text).unwrap();
        assert_eq!(v, "PASS");
        assert_eq!(r, "ok");
    }

    #[test]
    fn parse_verdict_tolerates_leading_prose() {
        let text = "Here is my verdict:\n{\"verdict\": \"FAIL\", \"reason\": \"x\"}";
        let (v, r) = parse_verdict(text).unwrap();
        assert_eq!(v, "FAIL");
        assert_eq!(r, "x");
    }

    #[test]
    fn parse_verdict_missing_verdict_is_error() {
        let text = r#"{"reason": "no verdict field"}"#;
        let result = parse_verdict(text);
        assert!(result.is_err(), "Missing verdict should be an error");
    }

    #[test]
    fn parse_verdict_falls_back_to_pass_gate() {
        let (v, _) = parse_verdict(r#"{"pass_gate": true}"#).unwrap();
        assert_eq!(v, "PASS");
        let (v, _) = parse_verdict(r#"{"pass_gate": false, "reason": "nope"}"#).unwrap();
        assert_eq!(v, "FAIL");
    }

    #[test]
    fn parse_verdict_falls_back_to_validation_passed() {
        let (v, _) = parse_verdict(r#"{"validation_passed": true}"#).unwrap();
        assert_eq!(v, "PASS");
    }

    #[test]
    fn parse_verdict_synthesizes_missing_reason() {
        let text = r#"{"verdict": "FAIL"}"#;
        let (v, r) = parse_verdict(text).unwrap();
        assert_eq!(v, "FAIL");
        assert!(
            r.contains("FAIL"),
            "fallback reason should include serialized object"
        );
    }

    #[test]
    fn load_workflow_from_yaml() {
        let yaml = r#"
name: "test_workflow"
description: "A test workflow"
parameters:
  type: object
  properties:
    task:
      type: string
  required: ["task"]
start_step: "step1"
steps:
  step1:
    prompt: "Do {{input.task}}"
    next: "step2"
  step2:
    prompt: "Check {{step1}}"
    evaluate:
      on_pass: "DONE"
      on_fail: "step1"
      max_retries: 2
"#;
        let manifest: WorkflowManifest = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(manifest.name, "test_workflow");
        assert_eq!(manifest.start_step, "step1");
        assert_eq!(manifest.steps.len(), 2);
        assert_eq!(
            manifest.steps["step2"]
                .evaluate
                .as_ref()
                .unwrap()
                .max_retries,
            2
        );
    }

    #[test]
    fn load_real_dev_yaml_has_output_template() {
        let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let manifest =
            load_workflow(&crate_root.join("../../workspace/workflows/dev.yaml")).unwrap();
        assert!(
            manifest.output_template.is_some(),
            "dev.yaml should have output_template"
        );
        let tmpl = manifest.output_template.unwrap();
        assert!(tmpl.contains("Dev Workflow Result"));
        assert!(tmpl.contains("{{review.reason}}"));
        assert!(tmpl.contains("{{implement}}"));
    }

    #[test]
    fn workflow_tool_name_and_description() {
        let manifest = WorkflowManifest {
            name: "my_flow".to_string(),
            description: "does things".to_string(),
            parameters: serde_json::json!({"type": "object", "properties": {}}),
            start_step: "a".to_string(),
            output_template: None,
            mode: WorkflowMode::Tool,
            always: true,
            steps: {
                let mut m = HashMap::new();
                m.insert(
                    "a".to_string(),
                    WorkflowStepDef {
                        prompt: "hello".to_string(),
                        model: None,
                        next: Some("DONE".to_string()),
                        evaluate: None,
                        tools: None,
                    },
                );
                m
            },
        };
        let workflow = Workflow::from_manifest(&manifest).unwrap();
        let tool = WorkflowTool::new(workflow);
        assert_eq!(tool.name(), "my_flow");
        assert_eq!(tool.description(), "does things");
    }

    #[test]
    fn to_skill_content_basic() {
        let yaml = r#"
name: "test_workflow"
description: "A test workflow"
mode: "skill"
parameters:
  type: object
  properties:
    task:
      type: string
      description: "What to do"
start_step: "step1"
steps:
  step1:
    prompt: "Do {{input.task}}"
    next: "step2"
  step2:
    prompt: "Check {{step1}}"
    evaluate:
      on_pass: "DONE"
      on_fail: "step1"
      max_retries: 2
"#;
        let manifest: WorkflowManifest = serde_yaml::from_str(yaml).unwrap();
        let workflow = Workflow::from_manifest(&manifest).unwrap();
        let content = workflow.to_skill_content();

        assert!(content.contains("## Workflow: test_workflow"));
        assert!(content.contains("A test workflow"));
        assert!(content.contains("#### 1. step1"));
        assert!(content.contains("#### 2. step2"));
        assert!(
            content.contains("[see above]"),
            "Template placeholders should be cleaned"
        );
        assert!(content.contains("On pass proceed to: **DONE**"));
        assert!(content.contains("On fail return to: **step1** (max retries: 2)"));
    }

    #[test]
    fn to_skill_content_dev_workflow() {
        let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let manifest =
            load_workflow(&crate_root.join("../../workspace/workflows/dev.yaml")).unwrap();
        assert_eq!(manifest.mode, WorkflowMode::Skill);

        let workflow = Workflow::from_manifest(&manifest).unwrap();
        let content = workflow.to_skill_content();
        assert!(content.contains("## Workflow: dev_workflow"));
        assert!(content.contains("#### 1. research"));
        assert!(content.contains("#### 2. plan"));
        assert!(content.contains("#### 3. implement"));
        assert!(content.contains("#### 4. review"));
        assert!(content.contains("On pass proceed to: **DONE**"));
        assert!(content.contains("On fail return to: **implement** (max retries: 3)"));
    }

    #[test]
    fn manifest_rejects_unknown_top_level_field() {
        let yaml = r#"
name: "x"
description: "y"
parameters: {type: object, properties: {}}
start_step: "a"
condition: "leftover_field"
steps:
  a:
    prompt: "p"
    next: "DONE"
"#;
        let result: Result<WorkflowManifest, _> = serde_yaml::from_str(yaml);
        assert!(
            result.is_err(),
            "unknown top-level field should be rejected"
        );
    }

    #[test]
    fn manifest_rejects_unknown_evaluate_field() {
        let yaml = r#"
name: "x"
description: "y"
parameters: {type: object, properties: {}}
start_step: "a"
steps:
  a:
    prompt: "p"
    evaluate:
      on_pass: "DONE"
      on_fail: "DONE"
      condition: "x > 0"
"#;
        let result: Result<WorkflowManifest, _> = serde_yaml::from_str(yaml);
        assert!(
            result.is_err(),
            "unknown field on evaluate config should be rejected"
        );
    }

    #[test]
    fn mode_defaults_to_tool_when_omitted() {
        let yaml = r#"
name: "x"
description: "y"
parameters: {type: object, properties: {}}
start_step: "a"
steps:
  a:
    prompt: "p"
    next: "DONE"
"#;
        let manifest: WorkflowManifest = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(manifest.mode, WorkflowMode::Tool);
        assert!(manifest.always, "always should default to true");
    }

    #[test]
    fn mode_rejects_invalid_string() {
        let yaml = r#"
name: "x"
description: "y"
parameters: {type: object, properties: {}}
start_step: "a"
mode: "skil"
steps:
  a:
    prompt: "p"
    next: "DONE"
"#;
        let result: Result<WorkflowManifest, _> = serde_yaml::from_str(yaml);
        assert!(result.is_err(), "typo in mode should be rejected");
    }

    #[test]
    fn from_manifest_rejects_unreachable_step() {
        let yaml = r#"
name: "x"
description: "y"
parameters: {type: object, properties: {}}
start_step: "a"
steps:
  a:
    prompt: "p"
    next: "DONE"
  orphan:
    prompt: "never used"
    next: "DONE"
"#;
        let manifest: WorkflowManifest = serde_yaml::from_str(yaml).unwrap();
        let result = Workflow::from_manifest(&manifest);
        let err = result.err().expect("unreachable step should be an error");
        assert!(
            err.to_string().contains("unreachable"),
            "error message should mention unreachable: {}",
            err
        );
    }

    #[test]
    fn from_manifest_accepts_on_fail_only_branches() {
        // self-evolution.yaml shape: `diagnose` and `refine` are only
        // reachable via evaluate.on_fail of `evaluate` / `validate`.
        let yaml = r#"
name: "selfev"
description: "y"
parameters: {type: object, properties: {}}
start_step: "execute"
steps:
  execute:
    prompt: "do work"
    next: "evaluate"
  evaluate:
    prompt: "score"
    evaluate:
      on_pass: "validate"
      on_fail: "diagnose"
      max_retries: 3
  diagnose:
    prompt: "why fail"
    next: "refine"
  refine:
    prompt: "fix"
    next: "validate"
  validate:
    prompt: "verify"
    evaluate:
      on_pass: "DONE"
      on_fail: "diagnose"
      max_retries: 3
"#;
        let manifest: WorkflowManifest = serde_yaml::from_str(yaml).unwrap();
        let workflow = Workflow::from_manifest(&manifest)
            .expect("workflow with on_fail-only branches should validate");
        assert_eq!(workflow.steps.len(), 5);
    }
}
