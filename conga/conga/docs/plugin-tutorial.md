# Writing a conga Extension Crate

An **extension** adds tools or hooks without changing `agent_loop`. It is a
normal Rust crate (workspace / path dependency) that exports:

```rust
pub fn register(api: &mut dyn ExtensionApi) {
    // api.register_tool / register_before_tool_call / ...
}
```

The **host binary** is the composition root: it calls each linked crate's
`register` at startup, often behind Cargo features. There is **no** `.so`
loading, no ABI version, no hot-unload.

Official examples: workspace crate **`conga-ext`** (`hello`, `search`,
`terminal`). `terminal` is gated behind the `terminal`
Cargo feature. CLI: `cargo run -p conga-cli --features ext`.

Built-in tools (`read` / `write` / `edit` / `bash` / `list` / `grep` /
`fetch` / `todo` / `spawn_subagents`) live in `conga-host`
(`conga_host::built_in_tools`) and are not extension crates.

---

## Host wiring

```rust
let mut api = ExtensionApiImpl::new();
conga_ext::register_all(&mut api); // or hello::register / search::register

let mut tools = conga_host::built_in_tools();
tools.extend(std::mem::take(&mut api.tools));

let config = AgentLoopConfig {
    hooks: Some(Arc::new(api)), // if hooks were registered
    // ...
};
```

For process-out hooks (external commands, Claude-compatible protocol) — see
[hooks.md](hooks.md) — no Rust crate required; configure via `hooks.json`.

Optional capabilities = optional **dependencies + features**, then recompile.
That is the static-world substitute for a plugin marketplace.

---

## The `ExtensionApi` surface

| Method | What it does | Example |
|---|---|---|
| `register_tool(ToolDefinition)` | add a tool the LLM may call | `hello` |
| `register_before_tool_call(handler)` | block / modify args before run | `conga_host::PermissionPolicy` |
| `register_after_tool_call(handler)` | rewrite tool result | — |

`before_tool_call` returns `ToolCallVerdict` (`Allow` / `Block` / `Modify`).
Extensions do **not** observe events or read session state through this
trait — event observation is the host's job (`HookChain`, storage).

---

## Example 1: `hello` — minimum extension

Source: `conga-ext/src/hello.rs`.

```rust
pub fn register(api: &mut dyn ExtensionApi) {
    api.register_tool(ToolDefinition {
        name: "hello".into(),
        label: "Hello".into(),
        description: "Say hello to someone.".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": { "name": { "type": "string" } },
            "required": ["name"]
        }),
        risk: RiskLevel::Low,
        execute: Arc::new(|ctx| Box::pin(async move {
            let name = ctx.args["name"].as_str().unwrap_or("world");
            Ok(ToolResult {
                content: vec![ContentBlock::text(format!("Hello, {}!", name))],
                details: serde_json::json!({ "greeted": name }),
                is_error: false,
            })
        })),
    });
}
```

- `parameters` is JSON Schema.
- `risk: RiskLevel` is **required** (no default) — set it honestly; the
  host's permission matrix keys off it.
- `details` is extension-private; the agent never reads it.

---

## Example 2: `todo` - private state files

`todo` was the original extension demo but has since moved to the built-in
tool set (`conga-host/src/tools/todo.rs`); the `state_dir` pattern it uses
applies to extension tools too. State lives under `ToolContext.state_dir`
(`~/.conga/sessions/<session_id>/tool_state/<tool_name>/`), not in a shared map.

---

## Example 3: `before_tool_call` hooks — policy gates

`before_tool_call` can `Block` or `Modify` a tool call before it runs; the
loop skips execution and returns the reason to the model as an error tool
result. Production CLI uses `conga_host::PermissionPolicy` (approval
prompts, risk-based auto-allow) as a real `HookChain` — that is the actual
security boundary for tool execution, not a string-matching extension hook.
A pattern-matching `before_tool_call` gate on `bash` command text is easy to
bypass (whitespace, `$()`, indirection) and should not be treated as one.

---

## Optional Cargo feature pattern

```toml
# conga-cli already wires this:
conga-ext = { workspace = true, optional = true }
[features]
ext = ["dep:conga-ext", "conga-ext?/terminal"]
```

```rust
#[cfg(feature = "ext")]
{
    conga_ext::hello::register(&mut api);
    conga_ext::search::register(&mut api);
}
```

Do **not** split built-in tools into per-tool features.

---

## Run the examples

```bash
cargo test -p conga-ext
```

---

## External tools (non-Rust)

For any language, host spawns a long-lived process and speaks JSONL on stdio
(`conga_host::ExternalToolBridge`). Example: `examples/external_echo.py`.

```bash
export CONGA_EXTERNAL_TOOLS="python3 path/to/external_echo.py"
# in REPL: /reload-tools  # kill + re-list (in-process Rust extensions do not reload)
```

Protocol: `{"op":"list"}` / `{"op":"call",...}` — one JSON object per line.
Does **not** expose `ExtensionApi` over the wire.

---

## Summary

- Extension = `pub fn register(&mut dyn ExtensionApi)` in a normal Rust crate.
- Host links crates and calls `register` (features optional).
- Built-ins stay in `conga-host`; no cdylib, no ABI handshake, no unload.
- Non-Rust tools: stdio JSONL external process + optional `/reload-tools`.
