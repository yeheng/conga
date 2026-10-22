//! JS extension scripts (feature `ext-js`): a QuickJS sandbox so extensions
//! can be written in JavaScript instead of compiled Rust crates.
//!
//! A script sees ONE global object, `conga` (defined by [`PRELUDE`]):
//! - `conga.registerTool(def)`          → a tool the LLM may call
//! - `conga.registerBeforeToolCall(fn)` → sync hook: allow / block / modify
//! - `conga.registerAfterToolCall(fn)`  → sync hook: rewrite a tool result
//!
//! Sandboxing: no module loader, no IO, no timers — plain JS only, plus a
//! memory and stack limit. Everything a script can touch lives in this file.
//! The permission gate is NOT bypassable from here: script tools go through
//! the same `before_tool_call` approval path as every other tool, and hooks
//! registered by a script can only add verdicts, never remove the policy's.
//!
//! Threading: QuickJS contexts are single-threaded and `!Send`, but tools
//! execute on tokio workers. All JS lives on one dedicated thread; requests
//! cross a channel, replies come back on a one-shot channel. Tool calls are
//! dispatched BY NAME — the `ToolFn` closure holds no JS values, so nothing
//! with a `'js` lifetime ever escapes the runtime thread. Script hooks must
//! be pure and fast: `before_tool_call` parks the agent loop on a blocking
//! recv while QuickJS runs the hook.

use std::path::PathBuf;
use std::sync::{mpsc, Arc};

use conga::extension::api::{AfterToolCallHandler, BeforeToolCallHandler};
use conga::{
    ContentBlock, ExtensionApiImpl, HookChain, RiskLevel, ToolCallCtx, ToolCallVerdict,
    ToolDefinition, ToolError, ToolResult, ToolResultMessage,
};

/// Heap cap for the script runtime.
const SANDBOX_MEMORY_LIMIT: usize = 64 * 1024 * 1024;
/// Stack cap for the script runtime.
const SANDBOX_STACK_LIMIT: usize = 1024 * 1024;

/// Registered before any user script: a registrations collector and a
/// stderr console. Plain `var`s so the collector arrays are reachable as
/// globals from Rust after all scripts ran.
const PRELUDE: &str = r#"
"use strict";
var __conga_tools = [], __conga_before = [], __conga_after = [];
var conga = {
    registerTool(def) { __conga_tools.push(def); },
    registerBeforeToolCall(fn) { __conga_before.push(fn); },
    registerAfterToolCall(fn) { __conga_after.push(fn); },
};
var console = {
    log(...a) { __conga_print("[ext-js]", ...a); },
    error(...a) { __conga_print("[ext-js error]", ...a); },
};
"#;

/// What the JS thread produces during setup, before it enters its request
/// loop. Everything here is `'static` — JS values stay behind on the thread.
pub struct JsExtensions {
    /// Tools registered by all scripts, in script order.
    pub tools: Vec<ToolDefinition>,
    /// The hook chain wrapping every script hook (empty chain if none).
    pub hooks: Arc<dyn HookChain>,
}

/// Tool-context subset a script's `execute(args, ctx)` receives. Deliberately
/// narrower than [`ToolContext`]: no env (may carry secrets).
struct JsToolCtx {
    cwd: PathBuf,
    session_id: String,
    state_dir: PathBuf,
}

/// One request to the JS runtime thread. Each variant carries its own
/// reply channel so the sender can block on exactly this request.
enum JsRequest {
    CallTool {
        name: String,
        args: serde_json::Value,
        ctx: JsToolCtx,
        reply: mpsc::SyncSender<Result<ToolResult, String>>,
    },
    BeforeHook {
        tool_name: String,
        args: serde_json::Value,
        risk: RiskLevel,
        reply: mpsc::SyncSender<ToolCallVerdict>,
    },
    AfterHook {
        tool_name: String,
        result: ToolResultMessage,
        reply: mpsc::SyncSender<ToolResultMessage>,
    },
}

/// Load JS extension scripts into tools + a hook chain.
///
/// Scripts run in registration order at load time; a script that throws
/// aborts loading with its exception (fail loud at startup, same as a
/// config error).
pub fn load_scripts(paths: &[PathBuf]) -> Result<JsExtensions, String> {
    if paths.is_empty() {
        return Ok(JsExtensions {
            tools: Vec::new(),
            hooks: Arc::new(ExtensionApiImpl::new()),
        });
    }
    let (tx, rx) = mpsc::channel::<JsRequest>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<JsExtensions, String>>();

    let paths = paths.to_vec();
    std::thread::Builder::new()
        .name("conga-ext-js".into())
        .spawn(move || js_thread(paths, tx, rx, ready_tx))
        .map_err(|e| format!("spawn ext-js thread: {e}"))?;

    ready_rx
        .recv()
        .map_err(|_| "ext-js thread died".to_string())?
}

/// The JS runtime thread: build the sandbox, eval the scripts, report the
/// registrations, then serve tool/hook requests until every sender drops.
fn js_thread(
    paths: Vec<PathBuf>,
    tx: mpsc::Sender<JsRequest>,
    rx: mpsc::Receiver<JsRequest>,
    ready: mpsc::Sender<Result<JsExtensions, String>>,
) {
    let run = || -> Result<(), String> {
        let rt = rquickjs::Runtime::new().map_err(|e| e.to_string())?;
        rt.set_memory_limit(SANDBOX_MEMORY_LIMIT);
        rt.set_max_stack_size(SANDBOX_STACK_LIMIT);
        let ctx = rquickjs::Context::full(&rt).map_err(|e| e.to_string())?;
        ctx.with(|ctx| -> Result<(), String> {
            let globals = ctx.globals();
            globals
                .set(
                    "__conga_print",
                    rquickjs::Function::new(
                        ctx.clone(),
                        |args: rquickjs::function::Rest<rquickjs::Value>| {
                            let parts: Vec<String> = args.iter().map(js_debug).collect();
                            eprintln!("{}", parts.join(" "));
                        },
                    ),
                )
                .map_err(|e| error_string(&ctx, e))?;
            ctx.eval::<(), _>(PRELUDE)
                .map_err(|e| error_string(&ctx, e))?;
            for path in &paths {
                let code = std::fs::read_to_string(path)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                ctx.eval::<(), _>(code)
                    .map_err(|e| format!("{}: {}", path.display(), error_string(&ctx, e)))?;
            }
            let registry = Registry::collect(&ctx)?;
            ready
                .send(Ok(build_extensions(&registry, tx.clone())))
                .map_err(|_| "loader dropped".to_string())?;
            serve(&ctx, &registry, &rx);
            Ok(())
        })
        .map_err(|e| e.to_string())
    };
    // A setup failure must reach the loader even when the thread is the only
    // remaining holder of `ready` — send the error, then let the thread end.
    if let Err(e) = run() {
        let _ = ready.send(Err(e));
    }
}

/// The `'js` values collected from script registrations, plus every static
/// metadata field already flattened to owned data. Lives only inside the
/// runtime thread's `ctx.with` scope.
struct Registry<'js> {
    tools: Vec<JsToolDef<'js>>,
    before: Vec<rquickjs::Function<'js>>,
    after: Vec<rquickjs::Function<'js>>,
}

/// A registered tool: static metadata (sent to the loader) + the execute
/// function (stays on the runtime thread).
struct JsToolDef<'js> {
    name: String,
    label: String,
    description: String,
    parameters: serde_json::Value,
    risk: RiskLevel,
    execute: rquickjs::Function<'js>,
}

impl<'js> Registry<'js> {
    fn collect(ctx: &rquickjs::Ctx<'js>) -> Result<Self, String> {
        let globals = ctx.globals();
        let collect_fn_array = |name: &str| -> Result<Vec<rquickjs::Function<'js>>, String> {
            let arr: rquickjs::Array = globals.get(name).map_err(|e| error_string(ctx, e))?;
            let mut out = Vec::new();
            for v in arr.iter::<rquickjs::Function<'js>>() {
                out.push(v.map_err(|e| error_string(ctx, e))?);
            }
            Ok(out)
        };
        let tools_arr: rquickjs::Array = globals
            .get("__conga_tools")
            .map_err(|e| error_string(ctx, e))?;
        let mut tools = Vec::new();
        for (i, def) in tools_arr.iter::<rquickjs::Object<'js>>().enumerate() {
            let def = def.map_err(|e| error_string(ctx, e))?;
            let name: String = def
                .get("name")
                .map_err(|e| format!("tool #{i}: {}", error_string(ctx, e)))?;
            let field = |key: &str| -> Result<String, String> {
                let v: Option<String> = def
                    .get(key)
                    .map_err(|e| format!("tool {name}.{key}: {}", error_string(ctx, e)))?;
                Ok(v.unwrap_or_default())
            };
            let label = field("label")?;
            let description = field("description")?;
            let risk = match field("risk")?.as_str() {
                "low" => RiskLevel::Low,
                "medium" => RiskLevel::Medium,
                // Default High: the grading rule says anything unexpected is
                // gated, never waved through.
                _ => RiskLevel::High,
            };
            let parameters = match def
                .get::<_, Option<rquickjs::Value>>("parameters")
                .map_err(|e| format!("tool {name}: {}", error_string(ctx, e)))?
            {
                Some(v) => js_to_json(&v),
                None => serde_json::json!({"type": "object", "properties": {}}),
            };
            let execute: rquickjs::Function<'js> = def
                .get("execute")
                .map_err(|e| format!("tool {name}: {}", error_string(ctx, e)))?;
            tools.push(JsToolDef {
                name,
                label,
                description,
                parameters,
                risk,
                execute,
            });
        }
        Ok(Self {
            tools,
            before: collect_fn_array("__conga_before")?,
            after: collect_fn_array("__conga_after")?,
        })
    }
}

/// Bridge the collected registrations into `'static` public types: one
/// `ToolDefinition` per script tool (its execute goes through the channel,
/// by name) and hook handlers on the shared chain.
fn build_extensions(registry: &Registry<'_>, tx: mpsc::Sender<JsRequest>) -> JsExtensions {
    let bridge = JsBridge { tx: tx.clone() };
    let mut api = ExtensionApiImpl::new();
    if !registry.before.is_empty() {
        api.before_hooks.push(Box::new(bridge.clone()));
    }
    if !registry.after.is_empty() {
        api.after_hooks.push(Box::new(bridge));
    }
    let tools = registry
        .tools
        .iter()
        .map(|t| ToolDefinition {
            name: t.name.clone(),
            label: t.label.clone(),
            description: t.description.clone(),
            parameters: t.parameters.clone(),
            risk: t.risk,
            execute: make_tool_execute(tx.clone(), t.name.clone()),
        })
        .collect();
    JsExtensions {
        tools,
        hooks: Arc::new(api),
    }
}

/// The `ToolFn` for a script tool: channel the call to the runtime thread
/// by name; block on the reply in the blocking pool, never on a worker.
fn make_tool_execute(tx: mpsc::Sender<JsRequest>, name: String) -> conga::ToolFn {
    Arc::new(move |call: ToolCallCtx| {
        let aborted = call.aborted();
        let tx = tx.clone();
        let name = name.clone();
        Box::pin(async move {
            if aborted {
                return Ok(ToolResult::error("aborted"));
            }
            let (reply_tx, reply_rx) = mpsc::sync_channel(1);
            tx.send(JsRequest::CallTool {
                name,
                args: call.args,
                ctx: JsToolCtx {
                    cwd: call.ctx.cwd,
                    session_id: call.ctx.session_id,
                    state_dir: call.ctx.state_dir,
                },
                reply: reply_tx,
            })
            .map_err(|_| ToolError::Message("ext-js runtime unavailable".into()))?;
            tokio::task::spawn_blocking(move || reply_rx.recv())
                .await
                .map_err(|e| ToolError::Message(format!("ext-js join: {e}")))?
                .map_err(|_| ToolError::Message("ext-js reply dropped".into()))?
                .map_err(ToolError::Message)
        })
    })
}

/// Sender side used by the `'static` tool closures and hook handlers.
#[derive(Clone)]
struct JsBridge {
    tx: mpsc::Sender<JsRequest>,
}

impl JsBridge {
    fn request<T>(&self, make: impl FnOnce(mpsc::SyncSender<T>) -> JsRequest) -> Option<T> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.tx.send(make(reply_tx)).ok()?;
        reply_rx.recv().ok()
    }
}

impl BeforeToolCallHandler for JsBridge {
    fn call(
        &self,
        _tool_call_id: &str,
        tool_name: &str,
        args: &serde_json::Value,
        risk: RiskLevel,
    ) -> ToolCallVerdict {
        self.request(|reply| JsRequest::BeforeHook {
            tool_name: tool_name.to_string(),
            args: args.clone(),
            risk,
            reply,
        })
        // Runtime gone: fail open (the permission policy still applies;
        // script hooks are additive, never load-bearing).
        .unwrap_or(ToolCallVerdict::Allow)
    }
}

impl AfterToolCallHandler for JsBridge {
    fn call(&self, _tool_call_id: &str, result: &ToolResultMessage) -> Option<ToolResultMessage> {
        self.request(|reply| JsRequest::AfterHook {
            tool_name: result.tool_name.clone(),
            result: result.clone(),
            reply,
        })
    }
}

/// Serve requests until every `JsBridge` drops. Runs inside `ctx.with`, so
/// all JS values here share the context's lifetime.
fn serve<'js>(ctx: &rquickjs::Ctx<'js>, registry: &Registry<'js>, rx: &mpsc::Receiver<JsRequest>) {
    while let Ok(req) = rx.recv() {
        match req {
            JsRequest::CallTool {
                name,
                args,
                ctx: tool_ctx,
                reply,
            } => {
                let _ = reply.send(call_tool(ctx, registry, &name, args, tool_ctx));
            }
            JsRequest::BeforeHook {
                tool_name,
                args,
                risk,
                reply,
            } => {
                let _ = reply.send(run_before_hooks(ctx, registry, &tool_name, args, risk));
            }
            JsRequest::AfterHook {
                tool_name,
                result,
                reply,
            } => {
                let _ = reply.send(run_after_hooks(ctx, registry, &tool_name, result));
            }
        }
    }
}

fn call_tool<'js>(
    ctx: &rquickjs::Ctx<'js>,
    registry: &Registry<'js>,
    name: &str,
    args: serde_json::Value,
    tool_ctx: JsToolCtx,
) -> Result<ToolResult, String> {
    let execute = registry
        .tools
        .iter()
        .find(|t| t.name == name)
        .map(|t| t.execute.clone())
        .ok_or_else(|| format!("no such js tool: {name}"))?;
    let js_args = json_to_js(ctx, &args).map_err(|e| error_string(ctx, e))?;
    let obj = rquickjs::Object::new(ctx.clone()).map_err(|e| error_string(ctx, e))?;
    obj.set("cwd", tool_ctx.cwd.to_string_lossy().to_string())
        .map_err(|e| error_string(ctx, e))?;
    obj.set("sessionId", tool_ctx.session_id.clone())
        .map_err(|e| error_string(ctx, e))?;
    obj.set("stateDir", tool_ctx.state_dir.to_string_lossy().to_string())
        .map_err(|e| error_string(ctx, e))?;
    let out: rquickjs::Value = match execute.call((js_args, obj.into_value())) {
        Ok(v) => v,
        // A JS throw is the script reporting an error to the LLM — surface
        // it as an error ToolResult, the same way denied host tools do.
        Err(e) => return Ok(ToolResult::error(error_string(ctx, e))),
    };
    tool_result_from_js(ctx, out)
}

/// `execute` returns a string, or `{ content?, isError?, details? }`.
fn tool_result_from_js(
    ctx: &rquickjs::Ctx<'_>,
    value: rquickjs::Value<'_>,
) -> Result<ToolResult, String> {
    if let Some(s) = value.as_string() {
        let text = s.to_string().map_err(|e| error_string(ctx, e))?;
        return Ok(ToolResult::text(text));
    }
    if value.type_of() == rquickjs::Type::Object {
        let obj = value.as_object().unwrap();
        let content: Option<String> = obj.get("content").unwrap_or(None);
        let is_error: bool = obj.get("isError").unwrap_or(false);
        let details: Option<rquickjs::Value> = obj.get("details").unwrap_or(None);
        return Ok(ToolResult {
            content: vec![ContentBlock::text(content.unwrap_or_default())],
            details: details
                .map(|v| js_to_json(&v))
                .unwrap_or(serde_json::Value::Null),
            is_error,
        });
    }
    // undefined / null / anything else → empty successful result.
    Ok(ToolResult::text(String::new()))
}

fn run_before_hooks<'js>(
    ctx: &rquickjs::Ctx<'js>,
    registry: &Registry<'js>,
    tool_name: &str,
    mut args: serde_json::Value,
    risk: RiskLevel,
) -> ToolCallVerdict {
    let mut modified = false;
    for hook in &registry.before {
        let payload = rquickjs::Object::new(ctx.clone()).expect("payload object");
        let _ = payload.set("toolName", tool_name);
        let _ = payload.set("args", json_to_js(ctx, &args).expect("args"));
        let _ = payload.set("risk", risk_name(risk));
        let verdict = match hook.call::<_, rquickjs::Value>((payload.into_value(),)) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[ext-js] before hook threw: {}", error_string(ctx, e));
                continue;
            }
        };
        if let Some(s) = verdict.as_string() {
            if matches!(s.to_string().as_deref(), Ok("allow")) {
                continue;
            }
        }
        if verdict.type_of() == rquickjs::Type::Object {
            let obj = verdict.as_object().unwrap();
            if let Ok(Some(reason)) = obj.get::<_, Option<String>>("block") {
                return ToolCallVerdict::Block(reason);
            }
            if let Ok(Some(replacement)) = obj.get::<_, Option<rquickjs::Value>>("modify") {
                args = js_to_json(&replacement);
                modified = true;
            }
        }
    }
    if modified {
        ToolCallVerdict::Modify(args)
    } else {
        ToolCallVerdict::Allow
    }
}

fn run_after_hooks<'js>(
    ctx: &rquickjs::Ctx<'js>,
    registry: &Registry<'js>,
    tool_name: &str,
    mut result: ToolResultMessage,
) -> ToolResultMessage {
    for hook in &registry.after {
        let text = result
            .content
            .iter()
            .find_map(|b| match b {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default();
        let payload = rquickjs::Object::new(ctx.clone()).expect("payload object");
        let _ = payload.set("toolName", tool_name);
        let _ = payload.set("text", text);
        let _ = payload.set("isError", result.is_error);
        let rewritten = match hook.call::<_, rquickjs::Value>((payload.into_value(),)) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[ext-js] after hook threw: {}", error_string(ctx, e));
                continue;
            }
        };
        if rewritten.type_of() != rquickjs::Type::Object {
            continue;
        }
        let obj = rewritten.as_object().unwrap();
        if let Ok(Some(text)) = obj.get::<_, Option<String>>("text") {
            result.content = vec![ContentBlock::text(text)];
        }
        if let Ok(Some(is_error)) = obj.get::<_, Option<bool>>("isError") {
            result.is_error = is_error;
        }
    }
    result
}

fn risk_name(risk: RiskLevel) -> &'static str {
    match risk {
        RiskLevel::Low => "low",
        RiskLevel::Medium => "medium",
        RiskLevel::High => "high",
    }
}

/// Turn a JS error into a string, consuming any pending exception (rquickjs
/// requires it before the context is used again).
fn error_string(ctx: &rquickjs::Ctx<'_>, err: rquickjs::Error) -> String {
    if matches!(err, rquickjs::Error::Exception) {
        let caught = ctx.catch();
        if let Some(ex) = caught.clone().into_exception() {
            let msg = ex.message().unwrap_or_else(|| "unknown error".into());
            if let Some(stack) = ex.stack() {
                return format!("{msg}\n{stack}");
            }
            return msg;
        }
        return format!("{:?}", caught);
    }
    err.to_string()
}

// ── serde_json ↔ QuickJS value conversion (both directions are total) ──

fn json_to_js<'js>(
    ctx: &rquickjs::Ctx<'js>,
    value: &serde_json::Value,
) -> rquickjs::Result<rquickjs::Value<'js>> {
    use rquickjs::IntoJs;
    match value {
        serde_json::Value::Null => Ok(rquickjs::Value::new_null(ctx.clone())),
        serde_json::Value::Bool(b) => b.into_js(ctx),
        serde_json::Value::Number(n) => n.as_f64().into_js(ctx),
        serde_json::Value::String(s) => s.into_js(ctx),
        serde_json::Value::Array(items) => {
            let arr = rquickjs::Array::new(ctx.clone())?;
            for (i, item) in items.iter().enumerate() {
                arr.set(i, json_to_js(ctx, item)?)?;
            }
            Ok(arr.into_value())
        }
        serde_json::Value::Object(map) => {
            let obj = rquickjs::Object::new(ctx.clone())?;
            for (k, v) in map {
                obj.set(k, json_to_js(ctx, v)?)?;
            }
            Ok(obj.into_value())
        }
    }
}

/// Debug rendering for `console.log`-style output on stderr.
fn js_debug(value: &rquickjs::Value<'_>) -> String {
    match value.as_string() {
        Some(s) => s.to_string().unwrap_or_else(|_| "<string>".into()),
        None => format!("{}", js_to_json(value)),
    }
}

fn js_to_json(value: &rquickjs::Value<'_>) -> serde_json::Value {
    match value.type_of() {
        rquickjs::Type::Bool => serde_json::Value::Bool(value.as_bool().unwrap_or(false)),
        rquickjs::Type::Int => serde_json::Value::from(value.as_int().unwrap_or(0)),
        rquickjs::Type::Float => serde_json::Value::from(value.as_float().unwrap_or(f64::NAN)),
        rquickjs::Type::String => serde_json::Value::String(
            value
                .as_string()
                .and_then(|s| s.to_string().ok())
                .unwrap_or_default(),
        ),
        rquickjs::Type::Array => {
            let arr = value.as_array().unwrap();
            let items: Vec<serde_json::Value> = arr
                .iter::<rquickjs::Value>()
                .flatten()
                .map(|v| js_to_json(&v))
                .collect();
            serde_json::Value::Array(items)
        }
        rquickjs::Type::Object => {
            let obj = value.as_object().unwrap();
            let mut map = serde_json::Map::new();
            for (k, v) in obj.props::<String, rquickjs::Value>().flatten() {
                map.insert(k, js_to_json(&v));
            }
            serde_json::Value::Object(map)
        }
        _ => serde_json::Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicBool;

    /// The acceptance criterion: a usable tool in ≤ 30 lines of script.
    const HELLO: &str = r#"
conga.registerTool({
    name: "js_hello",
    label: "JS Hello",
    description: "Greet someone.",
    parameters: { type: "object", properties: { name: { type: "string" } } },
    risk: "low",
    execute(args) { return "Hello, " + (args.name ?? "world") + "!"; },
});
"#;

    const BLOCKER: &str = r#"
conga.registerBeforeToolCall(({ toolName }) => {
    if (toolName === "bash") return { block: "no bash in demo" };
    if (toolName === "fetch") return { modify: { url: "https://example.com" } };
});
conga.registerAfterToolCall(({ isError }) => {
    if (isError) return { text: "[redacted error]", isError: true };
});
"#;

    fn write_script(dir: &tempfile::TempDir, name: &str, code: &str) -> PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, code).unwrap();
        path
    }

    fn tool_call_ctx(args: serde_json::Value) -> ToolCallCtx {
        ToolCallCtx {
            tool_call_id: "tc1".into(),
            args,
            signal: Arc::new(AtomicBool::new(false)),
            ctx: conga::ToolContext {
                cwd: "/tmp".into(),
                env: HashMap::new(),
                session_id: "sess".into(),
                state_dir: "/tmp/state".into(),
            },
        }
    }

    #[tokio::test]
    async fn registers_and_executes_a_tool() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(&dir, "hello.js", HELLO);
        let ext = load_scripts(&[script]).unwrap();
        assert_eq!(ext.tools.len(), 1);
        let tool = &ext.tools[0];
        assert_eq!(tool.name, "js_hello");
        assert_eq!(tool.risk, RiskLevel::Low);
        assert_eq!(tool.parameters["type"], "object");

        let result = (tool.execute)(tool_call_ctx(serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(result.content[0], ContentBlock::text("Hello, world!"));
        let result = (tool.execute)(tool_call_ctx(serde_json::json!({"name": "conga"})))
            .await
            .unwrap();
        assert_eq!(result.content[0], ContentBlock::text("Hello, conga!"));
    }

    #[tokio::test]
    async fn object_results_and_thrown_errors_map_to_tool_results() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(
            &dir,
            "shapes.js",
            r#"
conga.registerTool({
    name: "ok_shape", description: "", label: "",
    execute() { return { content: "done", isError: false, details: { n: 2 } }; },
});
conga.registerTool({
    name: "throws", description: "", label: "",
    execute() { throw new Error("boom"); },
});
"#,
        );
        let ext = load_scripts(&[script]).unwrap();
        let by_name = |n: &str| ext.tools.iter().find(|t| t.name == n).unwrap().clone();

        let ok = (by_name("ok_shape").execute)(tool_call_ctx(serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(ok.content[0], ContentBlock::text("done"));
        assert_eq!(ok.details["n"], 2);
        assert!(!ok.is_error);

        let err = (by_name("throws").execute)(tool_call_ctx(serde_json::json!({})))
            .await
            .unwrap();
        assert!(err.is_error);
        let ContentBlock::Text { text } = &err.content[0] else {
            panic!("expected a text block");
        };
        assert!(text.contains("boom"), "got: {text}");
    }

    #[tokio::test]
    async fn before_hooks_block_and_modify_after_hooks_rewrite() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(&dir, "hooks.js", BLOCKER);
        let ext = load_scripts(&[script]).unwrap();
        let hooks = ext.hooks;

        // Block wins.
        let verdict = hooks
            .before_tool_call(
                "tc",
                "bash",
                &serde_json::json!({"cmd": "ls"}),
                RiskLevel::High,
            )
            .await;
        match verdict {
            ToolCallVerdict::Block(reason) => assert_eq!(reason, "no bash in demo"),
            other => panic!("expected Block, got {other:?}"),
        }

        // Modify updates the args flowing to the LLM's tool call.
        let verdict = hooks
            .before_tool_call(
                "tc",
                "fetch",
                &serde_json::json!({"url": "https://evil.com"}),
                RiskLevel::Medium,
            )
            .await;
        match verdict {
            ToolCallVerdict::Modify(args) => assert_eq!(args["url"], "https://example.com"),
            other => panic!("expected Modify, got {other:?}"),
        }

        // Other tools pass through.
        let verdict = hooks
            .before_tool_call("tc", "read", &serde_json::json!({}), RiskLevel::Low)
            .await;
        assert!(matches!(verdict, ToolCallVerdict::Allow));

        // After-hook rewrites error results, leaves success alone.
        let make_result = |text: &str, is_error: bool| ToolResultMessage {
            tool_call_id: "tc".into(),
            tool_name: "bash".into(),
            content: vec![ContentBlock::text(text)],
            is_error,
            timestamp: 0,
        };
        let rewritten = hooks.after_tool_call("tc", &make_result("secret", true));
        assert_eq!(rewritten.content[0], ContentBlock::text("[redacted error]"));
        let untouched = hooks.after_tool_call("tc", &make_result("fine", false));
        assert_eq!(untouched.content[0], ContentBlock::text("fine"));
    }

    #[tokio::test]
    async fn a_throwing_script_fails_load_loud() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(&dir, "bad.js", "throw new Error('nope');");
        let err = match load_scripts(&[script]) {
            Err(e) => e,
            Ok(_) => panic!("a throwing script must fail load"),
        };
        assert!(err.contains("bad.js") && err.contains("nope"), "got: {err}");
    }

    #[tokio::test]
    async fn sandbox_has_no_loader_or_io() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(&dir, "escape.js", "require('fs');");
        let err = match load_scripts(&[script]) {
            Err(e) => e,
            Ok(_) => panic!("require must be undefined in the sandbox"),
        };
        assert!(err.contains("require"), "got: {err}");
    }

    // ── helpers for the direct (private API) tests ──

    /// A fresh QuickJS runtime with the sandbox prelude loaded.
    fn new_context() -> (rquickjs::Runtime, rquickjs::Context) {
        let rt = rquickjs::Runtime::new().unwrap();
        rt.set_memory_limit(SANDBOX_MEMORY_LIMIT);
        rt.set_max_stack_size(SANDBOX_STACK_LIMIT);
        let ctx = rquickjs::Context::full(&rt).unwrap();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(PRELUDE).unwrap();
        });
        (rt, ctx)
    }

    /// The tool-context subset a script receives.
    fn js_tool_ctx() -> JsToolCtx {
        JsToolCtx {
            cwd: PathBuf::from("/tmp"),
            session_id: "sess".into(),
            state_dir: PathBuf::from("/tmp/state"),
        }
    }

    fn text_of(result: &ToolResult) -> String {
        match result.content.first() {
            Some(ContentBlock::Text { text }) => text.clone(),
            other => panic!("expected a text block, got {other:?}"),
        }
    }

    // ── JSON ↔ JS value conversion ──

    #[test]
    fn json_to_js_converts_every_json_kind() {
        let (_rt, ctx) = new_context();
        ctx.with(|ctx| {
            let v = json_to_js(
                &ctx,
                &serde_json::json!({
                    "num": 1.5,
                    "str": "text",
                    "bool": true,
                    "nil": null,
                    "arr": [1.5, "x", null],
                    "obj": {"k": "v"}
                }),
            )
            .unwrap();
            let back = js_to_json(&v);
            assert_eq!(back["num"], 1.5);
            assert_eq!(back["str"], "text");
            assert_eq!(back["bool"], true);
            assert!(back["nil"].is_null());
            assert_eq!(back["arr"][0], 1.5);
            assert_eq!(back["arr"][1], "x");
            assert!(back["arr"][2].is_null());
            assert_eq!(back["obj"]["k"], "v");
        });
    }

    #[test]
    fn json_to_js_preserves_unicode_strings() {
        let (_rt, ctx) = new_context();
        ctx.with(|ctx| {
            let v = json_to_js(&ctx, &serde_json::json!("你好 🌍")).unwrap();
            assert_eq!(js_to_json(&v), serde_json::json!("你好 🌍"));
        });
    }

    /// KNOWN LIMITATION (reported, not fixed): every JSON number is bridged
    /// into JS through `f64`, so integers come back as floats and integers
    /// beyond 2^53 lose precision. This matters for the paths that round-trip
    /// JSON through JS: a before-hook's `modify` verdict and a tool result's
    /// `details`.
    ///
    /// This test pins the CURRENT behaviour; it must be updated when the
    /// bridge is fixed.
    #[test]
    fn json_numbers_travel_through_the_bridge_as_f64_known_limit() {
        let (_rt, ctx) = new_context();
        ctx.with(|ctx| {
            let v = json_to_js(
                &ctx,
                &serde_json::json!({"n": 42, "big": 9007199254740993u64}),
            )
            .unwrap();
            let back = js_to_json(&v);
            assert_eq!(back["n"], 42.0, "integers come back as floats");
            assert_eq!(
                back["big"], 9007199254740992.0,
                "integers beyond 2^53 lose precision"
            );
        });
    }

    #[test]
    fn js_to_json_maps_the_javascript_types() {
        let (_rt, ctx) = new_context();
        ctx.with(|ctx| {
            let v = ctx
                .eval::<rquickjs::Value, _>("({a: 1, b: 'x', c: true, d: null, e: [1, 2]})")
                .unwrap();
            let j = js_to_json(&v);
            assert_eq!(j["a"], 1);
            assert_eq!(j["b"], "x");
            assert_eq!(j["c"], true);
            assert!(j["d"].is_null());
            assert_eq!(j["e"].as_array().map(|a| a.len()), Some(2));
        });
    }

    #[test]
    fn js_to_json_of_undefined_is_null() {
        let (_rt, ctx) = new_context();
        ctx.with(|ctx| {
            let v = ctx.eval::<rquickjs::Value, _>("undefined").unwrap();
            assert!(js_to_json(&v).is_null());
        });
    }

    #[test]
    fn js_debug_renders_strings_and_values() {
        let (_rt, ctx) = new_context();
        ctx.with(|ctx| {
            let s = ctx.eval::<rquickjs::Value, _>("'hello'").unwrap();
            assert_eq!(js_debug(&s), "hello");
            let n = ctx.eval::<rquickjs::Value, _>("42").unwrap();
            assert_eq!(js_debug(&n), "42");
            let b = ctx.eval::<rquickjs::Value, _>("true").unwrap();
            assert_eq!(js_debug(&b), "true");
            let o = ctx.eval::<rquickjs::Value, _>("({a: 1})").unwrap();
            assert!(js_debug(&o).contains("\"a\":1"), "got: {}", js_debug(&o));
        });
    }

    // ── misc helpers ──

    #[test]
    fn risk_name_maps_the_three_levels() {
        assert_eq!(risk_name(RiskLevel::Low), "low");
        assert_eq!(risk_name(RiskLevel::Medium), "medium");
        assert_eq!(risk_name(RiskLevel::High), "high");
    }

    #[test]
    fn error_string_reads_a_pending_js_exception() {
        let (_rt, ctx) = new_context();
        ctx.with(|ctx| {
            let err = ctx
                .eval::<(), _>("throw new Error('boom')")
                .expect_err("must throw");
            let msg = error_string(&ctx, err);
            assert!(msg.contains("boom"), "got: {msg}");
        });
    }

    #[test]
    fn error_string_passes_a_non_exception_error_through() {
        let (_rt, ctx) = new_context();
        ctx.with(|ctx| {
            let msg = error_string(&ctx, rquickjs::Error::Allocation);
            assert!(!msg.is_empty());
        });
    }

    // ── registration collection ──

    #[test]
    fn registry_collect_reads_the_registered_tools_and_hooks() {
        let (_rt, ctx) = new_context();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(
                r#"
conga.registerTool({
    name: "t1", label: "L1", description: "D1", risk: "low",
    parameters: { type: "object", properties: { a: { type: "string" } } },
    execute(args) { return "ok"; },
});
conga.registerBeforeToolCall((p) => ({ block: "no" }));
conga.registerAfterToolCall((p) => ({ text: "x" }));
"#,
            )
            .unwrap();
            let registry = Registry::collect(&ctx).unwrap();
            assert_eq!(registry.tools.len(), 1);
            let t = &registry.tools[0];
            assert_eq!(t.name, "t1");
            assert_eq!(t.label, "L1");
            assert_eq!(t.description, "D1");
            assert_eq!(t.risk, RiskLevel::Low);
            assert_eq!(t.parameters["properties"]["a"]["type"], "string");
            assert_eq!(registry.before.len(), 1);
            assert_eq!(registry.after.len(), 1);
        });
    }

    #[test]
    fn registry_collect_defaults_the_missing_metadata() {
        let (_rt, ctx) = new_context();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(
                r#"
conga.registerTool({ name: "bare", execute() { return 1; } });
"#,
            )
            .unwrap();
            let registry = Registry::collect(&ctx).unwrap();
            let t = &registry.tools[0];
            assert_eq!(t.name, "bare");
            assert_eq!(t.label, "", "a missing label stays empty");
            assert_eq!(t.description, "");
            assert_eq!(t.risk, RiskLevel::High, "anything unexpected is High");
            assert_eq!(t.parameters["type"], "object");
            assert_eq!(t.parameters["properties"], serde_json::json!({}));
            assert!(registry.before.is_empty());
            assert!(registry.after.is_empty());
        });
    }

    // ── tool execution ──

    #[test]
    fn call_tool_runs_the_registered_function() {
        let (_rt, ctx) = new_context();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(
                r#"
conga.registerTool({
    name: "greet", label: "G", description: "D", risk: "low",
    execute(args) { return "hi " + (args.name ?? "world"); },
});
"#,
            )
            .unwrap();
            let registry = Registry::collect(&ctx).unwrap();
            let out = call_tool(
                &ctx,
                &registry,
                "greet",
                serde_json::json!({"name": "conga"}),
                js_tool_ctx(),
            )
            .unwrap();
            assert!(!out.is_error);
            assert_eq!(text_of(&out), "hi conga");
        });
    }

    #[test]
    fn call_tool_reports_an_unknown_tool() {
        let (_rt, ctx) = new_context();
        ctx.with(|ctx| {
            let registry = Registry::collect(&ctx).unwrap();
            let err = call_tool(
                &ctx,
                &registry,
                "nope",
                serde_json::json!({}),
                js_tool_ctx(),
            )
            .expect_err("must fail");
            assert!(err.contains("no such js tool: nope"), "got: {err}");
        });
    }

    #[test]
    fn call_tool_maps_a_thrown_error_to_an_error_result() {
        let (_rt, ctx) = new_context();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(
                r#"
conga.registerTool({ name: "boom", execute() { throw new Error("kaboom"); } });
"#,
            )
            .unwrap();
            let registry = Registry::collect(&ctx).unwrap();
            let out = call_tool(
                &ctx,
                &registry,
                "boom",
                serde_json::json!({}),
                js_tool_ctx(),
            )
            .unwrap();
            assert!(out.is_error, "a JS throw is an error result");
            assert!(text_of(&out).contains("kaboom"), "got: {}", text_of(&out));
        });
    }

    #[test]
    fn tool_result_from_js_reads_strings_objects_and_undefined() {
        let (_rt, ctx) = new_context();
        ctx.with(|ctx| {
            let v = ctx.eval::<rquickjs::Value, _>("'plain'").unwrap();
            let r = tool_result_from_js(&ctx, v).unwrap();
            assert_eq!(text_of(&r), "plain");
            assert!(!r.is_error);
            assert!(r.details.is_null());

            let v = ctx
                .eval::<rquickjs::Value, _>("({content: 'c', isError: true, details: {n: 1}})")
                .unwrap();
            let r = tool_result_from_js(&ctx, v).unwrap();
            assert_eq!(text_of(&r), "c");
            assert!(r.is_error);
            assert_eq!(r.details["n"], 1);

            let v = ctx.eval::<rquickjs::Value, _>("undefined").unwrap();
            let r = tool_result_from_js(&ctx, v).unwrap();
            assert_eq!(text_of(&r), "");
            assert!(!r.is_error);
        });
    }

    // ── hooks ──

    #[test]
    fn run_before_hooks_allows_blocks_and_modifies() {
        let (_rt, ctx) = new_context();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(
                r#"
conga.registerBeforeToolCall(({ toolName }) => {
    if (toolName === "bash") return { block: "no bash" };
    if (toolName === "fetch") return { modify: { url: "https://example.com" } };
    return "allow";
});
"#,
            )
            .unwrap();
            let registry = Registry::collect(&ctx).unwrap();

            match run_before_hooks(
                &ctx,
                &registry,
                "bash",
                serde_json::json!({}),
                RiskLevel::High,
            ) {
                ToolCallVerdict::Block(reason) => assert_eq!(reason, "no bash"),
                other => panic!("expected Block, got {other:?}"),
            }
            match run_before_hooks(
                &ctx,
                &registry,
                "fetch",
                serde_json::json!({"url": "https://evil.com"}),
                RiskLevel::Medium,
            ) {
                ToolCallVerdict::Modify(args) => assert_eq!(args["url"], "https://example.com"),
                other => panic!("expected Modify, got {other:?}"),
            }
            assert!(matches!(
                run_before_hooks(
                    &ctx,
                    &registry,
                    "read",
                    serde_json::json!({}),
                    RiskLevel::Low
                ),
                ToolCallVerdict::Allow
            ));
        });
    }

    #[test]
    fn run_before_hooks_survives_a_throwing_hook() {
        let (_rt, ctx) = new_context();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(
                r#"
conga.registerBeforeToolCall(() => { throw new Error("hook exploded"); });
"#,
            )
            .unwrap();
            let registry = Registry::collect(&ctx).unwrap();
            // The hook failed → the verdict stays Allow (the policy still applies).
            assert!(matches!(
                run_before_hooks(
                    &ctx,
                    &registry,
                    "bash",
                    serde_json::json!({}),
                    RiskLevel::High
                ),
                ToolCallVerdict::Allow
            ));
        });
    }

    #[test]
    fn run_after_hooks_rewrites_the_text_and_the_error_flag() {
        let (_rt, ctx) = new_context();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(
                r#"
conga.registerAfterToolCall(({ isError }) =>
    isError ? { text: "[redacted]", isError: true } : { text: "kept" });
"#,
            )
            .unwrap();
            let registry = Registry::collect(&ctx).unwrap();
            let make = |text: &str, is_error: bool| ToolResultMessage {
                tool_call_id: "tc1".into(),
                tool_name: "bash".into(),
                content: vec![ContentBlock::text(text)],
                is_error,
                timestamp: 0,
            };
            let rewritten = run_after_hooks(&ctx, &registry, "bash", make("secret", true));
            assert_eq!(rewritten.content[0], ContentBlock::text("[redacted]"));
            assert!(rewritten.is_error);

            let kept = run_after_hooks(&ctx, &registry, "bash", make("fine", false));
            assert_eq!(kept.content[0], ContentBlock::text("kept"));
            assert!(!kept.is_error);
        });
    }

    #[test]
    fn run_after_hooks_ignores_a_non_object_return() {
        let (_rt, ctx) = new_context();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(
                r#"
conga.registerAfterToolCall(() => "not an object");
"#,
            )
            .unwrap();
            let registry = Registry::collect(&ctx).unwrap();
            let result = ToolResultMessage {
                tool_call_id: "tc1".into(),
                tool_name: "bash".into(),
                content: vec![ContentBlock::text("unchanged")],
                is_error: false,
                timestamp: 0,
            };
            let out = run_after_hooks(&ctx, &registry, "bash", result);
            assert_eq!(out.content[0], ContentBlock::text("unchanged"));
        });
    }

    // ── bridging into the public types ──

    #[test]
    fn build_extensions_turns_the_registry_into_tools_and_hooks() {
        let (_rt, ctx) = new_context();
        ctx.with(|ctx| {
            ctx.eval::<(), _>(
                r#"
conga.registerTool({ name: "t1", label: "L1", description: "D1", risk: "medium",
                     execute() { return "ok"; } });
conga.registerBeforeToolCall(() => ({ block: "no" }));
conga.registerAfterToolCall(() => ({ text: "x" }));
"#,
            )
            .unwrap();
            let registry = Registry::collect(&ctx).unwrap();
            let (tx, _rx) = mpsc::channel::<JsRequest>();
            let ext = build_extensions(&registry, tx);
            assert_eq!(ext.tools.len(), 1);
            assert_eq!(ext.tools[0].name, "t1");
            assert_eq!(ext.tools[0].label, "L1");
            assert_eq!(ext.tools[0].description, "D1");
            assert_eq!(ext.tools[0].risk, RiskLevel::Medium);
        });
    }

    #[tokio::test]
    async fn build_extensions_fails_open_when_the_runtime_is_gone() {
        let (_rt, ctx) = new_context();
        let ext = ctx.with(|ctx| {
            ctx.eval::<(), _>(
                r#"
conga.registerBeforeToolCall(() => ({ block: "no" }));
conga.registerAfterToolCall(() => ({ text: "x" }));
"#,
            )
            .unwrap();
            let registry = Registry::collect(&ctx).unwrap();
            let (tx, rx) = mpsc::channel::<JsRequest>();
            let ext = build_extensions(&registry, tx);
            drop(rx); // the runtime thread is gone
            ext
        });

        // The before hook cannot reach the runtime → fail open.
        let verdict = ext
            .hooks
            .before_tool_call("tc", "bash", &serde_json::json!({}), RiskLevel::High)
            .await;
        assert!(matches!(verdict, ToolCallVerdict::Allow));

        // The after hook cannot reach the runtime → the result is kept.
        let result = ToolResultMessage {
            tool_call_id: "tc1".into(),
            tool_name: "bash".into(),
            content: vec![ContentBlock::text("unchanged")],
            is_error: false,
            timestamp: 0,
        };
        let out = ext.hooks.after_tool_call("tc", &result);
        assert_eq!(out.content[0], ContentBlock::text("unchanged"));
    }

    #[tokio::test]
    async fn make_tool_execute_dispatches_to_the_runtime_thread() {
        let (tx, rx) = mpsc::channel::<JsRequest>();
        let responder = std::thread::spawn(move || {
            let req = rx.recv().expect("a request must arrive");
            match req {
                JsRequest::CallTool { name, reply, .. } => {
                    let _ = reply.send(Ok(ToolResult::text("from js")));
                    name
                }
                _ => panic!("expected a CallTool request"),
            }
        });
        let exec = make_tool_execute(tx, "t1".to_string());
        let out = (exec)(tool_call_ctx(serde_json::json!({"a": 1})))
            .await
            .expect("must succeed");
        assert_eq!(out.content[0], ContentBlock::text("from js"));
        assert_eq!(
            responder.join().unwrap(),
            "t1",
            "the tool name is dispatched"
        );
    }

    #[tokio::test]
    async fn make_tool_execute_short_circuits_an_aborted_call() {
        let (tx, _rx) = mpsc::channel::<JsRequest>();
        let exec = make_tool_execute(tx, "t1".to_string());
        let mut ctx = tool_call_ctx(serde_json::json!({}));
        ctx.signal = Arc::new(AtomicBool::new(true));
        let out = (exec)(ctx).await.expect("must succeed");
        assert!(out.is_error);
        assert_eq!(out.content[0], ContentBlock::text("aborted"));
    }

    #[tokio::test]
    async fn make_tool_execute_reports_a_dead_runtime() {
        let (tx, rx) = mpsc::channel::<JsRequest>();
        drop(rx);
        let exec = make_tool_execute(tx, "t1".to_string());
        let err = (exec)(tool_call_ctx(serde_json::json!({})))
            .await
            .expect_err("must fail");
        assert!(
            err.to_string().contains("ext-js runtime unavailable"),
            "got: {err}"
        );
    }

    // ── the runtime thread ──

    #[test]
    fn js_thread_reports_a_failing_script() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(&dir, "bad.js", "throw new Error('nope');");
        let (tx, rx) = mpsc::channel::<JsRequest>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<JsExtensions, String>>();
        let handle = std::thread::spawn(move || js_thread(vec![script], tx, rx, ready_tx));
        let reported = ready_rx.recv().expect("the thread must report back");
        let err = match reported {
            Err(e) => e,
            Ok(_) => panic!("a throwing script must fail the load"),
        };
        assert!(err.contains("bad.js"), "got: {err}");
        assert!(err.contains("nope"), "got: {err}");
        handle.join().unwrap();
    }

    #[tokio::test]
    async fn js_thread_reports_the_registrations_and_serves_requests() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(
            &dir,
            "tool.js",
            r#"
conga.registerTool({ name: "x", label: "X", description: "D", risk: "low",
    execute() { return "ran"; } });
"#,
        );
        let (tx, rx) = mpsc::channel::<JsRequest>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<JsExtensions, String>>();
        let handle = std::thread::spawn(move || js_thread(vec![script], tx, rx, ready_tx));
        let ext = ready_rx
            .recv()
            .expect("the thread must report back")
            .expect("the script must load");
        assert_eq!(ext.tools.len(), 1);
        assert_eq!(ext.tools[0].name, "x");
        assert_eq!(ext.tools[0].risk, RiskLevel::Low);

        // The runtime is alive: the tool call round-trips through the thread.
        let out = (ext.tools[0].execute)(tool_call_ctx(serde_json::json!({})))
            .await
            .expect("must succeed");
        assert!(!out.is_error);
        assert_eq!(out.content[0], ContentBlock::text("ran"));

        // The runtime thread lives until its senders drop; detach it.
        drop(handle);
    }

    #[test]
    fn load_scripts_with_no_scripts_returns_no_tools() {
        let ext = load_scripts(&[]).expect("must load");
        assert!(ext.tools.is_empty());
    }

    #[test]
    fn load_scripts_reports_a_missing_script_file() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.js");
        let err = match load_scripts(&[missing]) {
            Err(e) => e,
            Ok(_) => panic!("a missing script file must fail the load"),
        };
        assert!(err.contains("missing.js"), "got: {err}");
    }
}
