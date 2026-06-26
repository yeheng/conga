# Module Design

> Gasket-RS Module Responsibilities and Interface Design

---

## 1. providers/ — LLM Provider Abstraction Layer

### Core Trait

```rust
#[async_trait]
trait LlmProvider: Send + Sync {
    fn name(&self) -> &str;
    fn default_model(&self) -> &str;
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse>;
    async fn chat_stream(&self, request: ChatRequest) -> Result<ChatStream>;
}
```

### Provider Implementations

```mermaid
flowchart TB
    TP["trait LlmProvider<br/>├── name()<br/>├── default_model()<br/>├── chat(ChatRequest)<br/>└── chat_stream()"]

    TP --> OP["OpenAICompatibleProvider"]
    TP --> GP["GeminiProvider"]
    TP --> CP["CopilotProvider"]

    OP --> ON["from_name()<br/>openai · openrouter · deepseek<br/>anthropic · zhipu · dashscope<br/>moonshot · minimax · ollama · litellm"]

    style TP fill:#E3F2FD
    style OP fill:#FFF3E0
    style GP fill:#FFF3E0
    style CP fill:#FFF3E0
```

- **OpenAICompatibleProvider**: Configured via `PROVIDER_DEFAULTS` table, adding a new provider only requires adding a row of data, no code needed
- **GeminiProvider**: Google Gemini API (non-OpenAI compatible format)
- **CopilotProvider**: GitHub Copilot API (with OAuth authentication flow)

**ModelSpec parsing format**: `provider_id/model_id` or `model_id`

| Input | provider | model |
|------|----------|-------|
| `deepseek/deepseek-chat` | `deepseek` | `deepseek-chat` |
| `anthropic/claude-sonnet-4` | `anthropic` | `claude-sonnet-4` |
| `gpt-4o` | `None` (use default) | `gpt-4o` |

---

## 2. tools/ — Tool System

### Core Trait

```rust
#[async_trait]
trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> serde_json::Value;  // JSON Schema
    async fn execute(&self, args: Value, ctx: &ToolContext) -> ToolResult;
}
```

### Built-in Tool List

| Tool | Category | Description |
|------|----------|-------------|
| `read_file` | filesystem | Read file content |
| `write_file` | filesystem | Write file |
| `edit_file` | filesystem | Edit file (search/replace) |
| `list_dir` | filesystem | List directory contents |
| `exec` | system | Execute shell command (with timeout + policy: allowlist/denylist) |
| `spawn` | system | Create subagent to execute task |
| `spawn_parallel` | system | Execute multiple tasks in parallel with subagents |
| `web_fetch` | web | HTTP GET request |
| `web_search` | web | Web search (Brave/Tavily/Exa/Firecrawl) |
| `MessageTool` | communication | Send message through Broker to channel |
| `cron` | system | Manage scheduled tasks (CRUD) |
| `ask_user` | communication | Ask the user a question and wait for response |
| `create_plan` | planning | Create a structured plan for a goal |
| `clear_session` | session | Clear current session history |
| `history_query` | history | Query conversation history by keywords |
| `history_search` | history | Semantic search through history |
| Plugin tools | plugin | External script tools loaded from `~/.gasket/plugins/` |

### Helper Modules

| Module | Description |
|------|-------------|
| `registry.rs` | `ToolRegistry` — Tool registry with semantic routing support |
| `base.rs` | Re-exports `Tool` trait, `ToolContext`, `ToolError` from types crate |

---

## 2.5. plugin/ — External Plugin System

> Located at `engine/src/plugin/`

The plugin system loads external scripts via YAML manifests and exposes them as native tools.

### Module Structure

| File | Responsibility |
|------|----------------|
| `mod.rs` | `PluginTool` — Tool trait implementation for external scripts |
| `manifest.rs` | `PluginManifest`, `PluginProtocol`, `RuntimeConfig`, `Permission` |
| `rpc.rs` | JSON-RPC 2.0 message types (`RpcMessage`, `RpcRequest`, `RpcResponse`) and line codec |
| `runner/simple.rs` | One-shot stdin/stdout runner for Simple protocol |
| `runner/jsonrpc.rs` | Bidirectional JSON-RPC runner |
| `runner/daemon.rs` | `JsonRpcDaemon` — persistent JSON-RPC process with request multiplexing |
| `dispatcher/mod.rs` | `RpcDispatcher` — routes RPC calls with permission enforcement |
| `dispatcher/llm_chat.rs` | Handler for `llm/chat` |
| `dispatcher/subagent.rs` | Handler for `subagent/spawn` |

### Protocols

- **Simple**: One-shot JSON input/output via stdin/stdout
- **JsonRpc**: Bidirectional JSON-RPC 2.0 with callback methods (`llm/chat`, etc.)

### Permissions (Default Deny)

| Permission | RPC Method |
|------------|------------|
| `LlmChat` | `llm/chat` |
| `SubagentSpawn` | `subagent/spawn` |
| `MessageSend` | `message/send` |
| `UserAsk` | `user/ask` |

---

## 3. channels/ — Communication Channels

### Core Trait

```rust
#[async_trait]
trait Channel: Send + Sync {
    fn name(&self) -> &str;
    async fn start(&mut self) -> Result<()>;  // Start receiving messages
    async fn stop(&mut self) -> Result<()>;   // Stop
    async fn graceful_shutdown(&mut self) -> Result<()>;
}
```

> Channel is **inbound-only**: receives external messages and pushes to internal Bus. All **outbound** sending is handled by Outbound Actor through `send_outbound()` function routing by channel type.

### Channel List

| Channel | Feature Flag | Transport Protocol | Description |
|------|-------------|----------|------|
| Telegram | `telegram` | Long Polling (teloxide) | Telegram Bot API |
| Discord | `discord` | WebSocket (serenity) | Discord Gateway |
| Slack | `slack` | WebSocket (tungstenite) | Slack Socket Mode |
| Feishu | `feishu` | HTTP Webhook (axum) | Feishu event subscription |
| WebSocket | `websocket` | WebSocket (axum) | Real-time bidirectional communication |

### Middleware Layer

| Component | Description |
|------|-------------|
| `SimpleAuthChecker` | Whitelist-based sender authentication |
| `SimpleRateLimiter` | Simple rate limiting |
| `InboundSender` | Encapsulates inbound message sending logic |
| `log_inbound` | Inbound message logging |

---

## 4. plugin/ — External Plugin System

> Detailed design: [plugin-en.md](plugin-en.md)

Gasket uses an external plugin system instead of MCP. Plugins are loaded from `~/.gasket/plugins/` via YAML manifests and exposed as native tools. See the [Plugin System documentation](plugin-en.md) for details.

---

## 5. broker/ — Message Broker

> Detailed design: [broker-module-design.md](broker-module-design-en.md)

### Core Abstractions

| Type | Responsibility |
|------|----------------|
| `Topic` | Strongly-typed topic enum (Inbound, Outbound, SystemEvent, ToolCall, etc.) |
| `DeliveryMode` | Compile-time decision: `PointToPoint` (work-stealing) or `Broadcast` (fan-out) |
| `Envelope` | Message wrapper: `id`, `timestamp`, `topic`, `payload` |
| `Subscriber` | Unified receiver: `PointToPoint` or `Broadcast` |

### Delivery Modes

```mermaid
flowchart LR
    subgraph TopicTypes
        TI["Topic::Inbound"]
        TO["Topic::Outbound"]
        TS["Topic::SystemEvent"]
        TC["Topic::ToolCall(String)"]
    end

    subgraph DeliveryMode
        P2P["PointToPoint<br/>(async_channel)"]
        BC["Broadcast<br/>(tokio::broadcast)"]
    end

    TI --> P2P
    TO --> P2P
    TS --> BC
    TC --> P2P

    style P2P fill:#E3F2FD
    style BC fill:#FFF3E0
```

### MemoryBroker Implementation

Uses DashMap + async-channel:
- `publish(envelope)` — Blocking publish with backpressure
- `try_publish(envelope)` — Non-blocking publish
- `subscribe(topic)` — Subscribe and return Subscriber
- `metrics(topic)` — Queue state snapshot

### SessionManager

Manages per-session message routing:
- Subscribes to `Topic::Inbound`
- Dispatches to per-session mpsc channels
- Idle timeout GC every 300s

---

## 6. hooks/ — Agent Pipeline Lifecycle Hook System

Unified pipeline extension mechanism with five execution points and sequential/parallel strategies.

### Hook Points

| Hook Point | Timing | Strategy | Description |
|------------|--------|----------|-------------|
| `BeforeRequest` | Before request processed | Sequential | Can modify input, can abort |
| `AfterHistory` | After history loaded | Sequential | Can add context |
| `BeforeLLM` | Before sending to LLM | Sequential | Last chance to modify |
| `AfterToolCall` | After tool call completes | Parallel | Read-only, fire-and-forget |
| `AfterResponse` | After response generated | Parallel | Audit/alert |

### Core Components

| Component | Responsibility |
|-----------|----------------|
| `HookRegistry` | Hook registry, manages all hooks by point |
| `PipelineHook` | Hook trait with `name()`, `point()`, `run()`, `run_parallel()` |
| `HookBuilder` | Builder for creating HookRegistry |
| `HookContext<M>` | Generic context with session_key, messages, user_input, response |
| `ExternalShellHook` | Shell script hook wrapper |
| `VaultHook` | Vault secret injection at BeforeLLM |

### External Shell Hooks

```mermaid
flowchart LR
    Rust -->|"stdin (JSON)"| Shell
    Shell -->|"stdout (JSON)"| Rust
    Shell -->|"stderr"| tracing["tracing::debug!"]
```

- Scripts located in `~/.gasket/hooks/`
- `pre_request.sh` — Request preprocessing (can modify or abort input)
- `post_response.sh` — Post-response processing (audit/alert)
- 2 second timeout, 1 MB stdout limit, non-blocking `tokio::process::Command`

---

## 7. storage/ — Storage Abstraction Layer

> **Note**: Implementation re-exported from `storage` crate.

### Core Components

| Component | Description |
|-----------|-------------|
| `EventStore` | Event sourcing storage (session_events table) |
| `SqliteStore` | SQLite general storage (sessions, summaries, cron jobs, kv) |
| `processor` | `process_history()` — Token-budget-aware history processing |
| `query` | `HistoryQueryBuilder` — History query builder |
| `search/` | FTS5 full-text search types |

### SqliteStore

- Uses `sqlx::SqlitePool` native async I/O
- WAL mode for concurrent reads
- Submodules: `fs.rs` (filesystem), `event_store.rs` (events)

---

## 8. Event Sourcing

> **Note**: Event sourcing types defined in `types` crate (`SessionEvent`, `EventType`, `Session`), persistence in `storage` crate (`EventStore`).

### Core Types (from types crate)

| Type | Description |
|------|-------------|
| `Session` | Aggregate root with metadata (created_at, updated_at, total_events) |
| `SessionEvent` | Immutable events with UUID v7, session_key, event_type, content, optional embedding |
| `EventType` | UserMessage, AssistantMessage, ToolCall, ToolResult, Summary |
| `SummaryType` | TimeWindow, Topic, Compression |
| `EventMetadata` | tools_used, token_usage, content_token_len, extra |
| `SessionMetadata` | created_at, updated_at, last_consolidated_event, total_events, total_tokens |

### Architecture

- **Event Sourcing**: All messages stored as immutable events enabling full history reconstruction
- **EventStore** (storage crate): `append_event()`, `get_events_after_watermark()`, `get_events_by_ids()`, `clear_session()`, `get_latest_summary()`
- **Pure SQLite**: No in-memory cache, reads directly from database, leverages SQLite page cache
- **History Processing**: `process_history()` with token budget, recent_keep, max_events configuration
- **Query System**: `HistoryQueryBuilder` with time_range, event_types, semantic_query, tools filters

---

## 9. session/ — Session Management

> **Note**: `engine/src/agent/` has been restructured into `kernel/` + `session/` + `subagents/`

| File/Directory | Responsibility |
|------|----------------|
| `mod.rs` | `AgentSession` — Session management core, wraps kernel execution |
| `config.rs` | `AgentConfig` — Agent configuration with kernel conversion support |
| `builder.rs` | `SessionBuilder` — Session builder |
| `finalizer.rs` | `ResponseFinalizer` — Response post-processor |
| `pending_ask.rs` | `PendingAskRegistry` — Pending ask registry |
| `prompt.rs` | Bootstrap file loading, skills context, token truncation |
| `history/` | Event sourcing history processing |
| `compactor/` | Context compression (based on token budget) |

### history/ Submodule

| File | Responsibility |
|------|----------------|
| `builder.rs` | `ContextBuilder` — History message builder |
| `indexing.rs` | `HistoryIndexingService` — Message indexing service |
| `mod.rs` | Module exports |

### AgentSession

`AgentSession` is the core session management structure that wraps kernel execution:

```rust
pub struct AgentSession {
    runtime_ctx: RuntimeContext,                              // Kernel execution context
    config: AgentConfig,                                      // Agent configuration
    context_builder: history::builder::ContextBuilder,         // History/memory assembly
    compactor: Option<Arc<ContextCompactor>>,            // Context compactor
    pricing: Option<ModelPricing>,                          // Optional pricing for cost calculation
    finalizer: ResponseFinalizer,                             // Response post-processor
    pending_done: tokio_util::task::TaskTracker,          // Graceful shutdown tracker
    pending_asks: Arc<PendingAskRegistryImpl>,            // Pending ask registry
    #[cfg(feature = "embedding")]
    embedding_indexer: Option<gasket_embedding::EmbeddingIndexer>, // Embedding indexer
}
```

---

## 10. kernel/ — Pure Function Execution Core

| File | Responsibility |
|------|----------------|
| `mod.rs` | `execute()`, `execute_streaming()` — Pure function execution entry points |
| `executor.rs` | `AgentExecutor`, `ToolExecutor`, `ExecutionResult` — Executor implementations |
| `context.rs` | `RuntimeContext`, `KernelConfig` — Runtime context and configuration |
| `stream.rs` | `StreamEvent`, `BufferedEvents` — Streaming output events |
| `error.rs` | `KernelError` — Kernel error types |

### Pure Function Execution Interface

```rust
/// Execute LLM conversation loop
pub async fn execute(
    ctx: &RuntimeContext,
    messages: Vec<ChatMessage>,
) -> Result<ExecutionResult, KernelError>;

/// Streaming LLM conversation loop
pub async fn execute_streaming(
    ctx: &RuntimeContext,
    messages: Vec<ChatMessage>,
    event_tx: mpsc::Sender<StreamEvent>,
) -> Result<ExecutionResult, KernelError>;
```

---

## 11. subagents/ — Subagent System

| File | Responsibility |
|------|----------------|
| `manager.rs` | `spawn_subagent()`, `TaskSpec` — Pure function subagent spawning |
| `tracker.rs` | `SubagentTracker`, `TrackerError` — Parallel task coordination |
| `runner.rs` | `ModelResolver` — Subagent execution and model resolution |

### Spawning API

Subagent spawning uses a simple pure-function approach:

```rust
let task = TaskSpec::new("sub-1", "Execute task")
    .with_model("openrouter/anthropic/claude-sonnet-4")
    .with_system_prompt("Custom prompt".to_string());

let handle = spawn_subagent(
    provider,
    tools,
    workspace,
    task,
    Some(event_tx),
    result_tx,
    Some(token_tracker),
    cancellation_token,
);
```

### Subagent Result

```rust
pub struct SubagentResult {
    pub id: String,              // Subagent ID
    pub task: String,            // Task description
    pub response: SubagentResponse, // Execution result
    pub model: Option<String>,   // Model name used
}
```

---

## 12. config/ — Configuration Management

| File | Responsibility |
|------|----------------|
| `mod.rs` | Configuration module exports |
| `app_config.rs` | Main `Config` struct, `ConfigLoader`, `ModelConfig`, `ModelProfile`, `ModelRegistry`, `ProviderConfig`, `ProviderRegistry`, `ProviderType` |
| `tools.rs` | `ToolsConfig`, `ExecToolConfig` (command policy), `WebToolsConfig` (search/fetch/proxy), `SandboxConfig`, `CommandPolicyConfig`, `ResourceLimitsConfig`, `EmbeddingConfig` |

- Config file at `~/.gasket/config.yaml`
- Compatible with Python gasket configuration format

---

## 13. vault/ — Sensitive Data Isolation Module (inside engine)

> Detailed usage guide in [vault-guide.md](vault-guide.md)

Vault module is located at `engine/src/vault/`, not a separate crate.

### Core Components

| File | Responsibility |
|------|----------------|
| `store.rs` | `VaultStore` — JSON file storage, supports encryption |
| `injector.rs` | `VaultInjector` — Runtime placeholder replacement |
| `scanner.rs` | Placeholder scanning and parsing (`{{vault:key}}`) |
| `crypto.rs` | `VaultCrypto` — XChaCha20-Poly1305 encryption |
| `redaction.rs` | Log redaction functions (`redact_secrets`) |
| `error.rs` | `VaultError` error types |

### Design Principles

1. **Data structure isolation** — VaultStore completely independent from memory/history storage
2. **Runtime injection** — Sensitive data injected only at the last moment before sending to LLM
3. **Zero-trust design** — Sensitive data never persisted to LLM-accessible storage

### Placeholder Syntax

```
Use {{vault:api_key}} to access API
Password: {{vault:db_password}}
```

---

## 14. search/ — Search & Embedding

> **Note**: Search types re-exported from `storage` crate. Advanced Tantivy full-text search in standalone `tantivy` crate.

### Core Types

| Type | Description |
|------|-------------|
| `TextEmbedder` | ONNX-based text embedding via fastembed (feature: local-onnx) |
| `EmbeddingConfig` | Model name, cache dir, local model path configuration |
| `cosine_similarity()` | Calculate cosine similarity between two vectors |
| `top_k_similar()` | Get top-K most similar items from vector collection |
| `bytes_to_embedding()` | Convert byte slice to embedding vector |
| `embedding_to_bytes()` | Convert embedding vector to byte slice |

### Semantic Search Pipeline

1. `TextEmbedder::embed(text) -> Vec<f32>` — Generate embedding for query
2. `cosine_similarity(query, candidate) -> f32` — Score similarity
3. `top_k_similar(query, vectors, k) -> Vec<(f32, String)>` — Rank results

### History Query Builder

```rust
let results = HistoryQuery::builder("session-key")
    .branch("main")
    .time_range(start, end)
    .event_types(vec!["UserMessage".into()])
    .semantic_text("search query")
    .tools(vec!["exec".into()])
    .limit(10)
    .order(QueryOrder::ReverseChronological)
    .build();
```

---

## 15. Other Modules

| Module | Description |
|------|-------------|
| `cron/` | `CronService` + `CronJob` — Scheduled task service, file-driven |
| `heartbeat/` | `HeartbeatService` — Reads HEARTBEAT.md, triggers periodic proactive tasks |
| `skills/` | Skills system — `SkillsLoader`, `SkillsRegistry`, `Skill`, `SkillMetadata` (see Section 16) |
| `bus_adapter.rs` | `EngineHandler` — Bridges engine to bus actor system |
| `error.rs` | Unified error types (AgentError, ProviderError, ChannelError, PipelineError, ConfigValidationError) |
| `token_tracker.rs` | Token counting, cost calculation, session stats tracking |

---

## 16. skills/ — Skills System

### Module Structure

| File | Responsibility |
|------|----------------|
| `loader.rs` | `SkillsLoader` — Load skills from Markdown files |
| `registry.rs` | `SkillsRegistry` — Skills registry management |
| `skill.rs` | `Skill` — Skill definition structure |
| `metadata.rs` | `SkillMetadata` — Skill metadata (name, description, bins, env_vars, always, extra) |

### Skill File Format

```markdown
---
name: my_skill
description: A sample skill
bins: ["node", "npm"]
env_vars: ["API_KEY"]
always_load: false
---

# My Skill

Detailed description and usage of the skill...
```

### Loading Modes

- **always_load: true** — Auto-load at startup
- **always_load: false** — Load on demand

---

## 18. tantivy/ — Standalone Tantivy CLI Tool

> Detailed design: [tantivy-module-design-en.md](tantivy-module-design-en.md)

Standalone CLI tool for managing multiple Tantivy full-text search indexes.

### CLI Commands

| Command | Description |
|---------|-------------|
| `index create/list/stats/drop/compact/rebuild` | Index management |
| `doc add/add-batch/delete/commit` | Document operations |
| `search` | Full-text search |

### Core Components

| Component | Responsibility |
|-----------|----------------|
| `IndexManager` | Multi-index management, HashMap in-memory registry |
| `FieldDef` | Field definition (Text, String, I64, DateTime, etc.) |
| `SearchQuery` | BM25 + filtering + pagination + highlighting |
| `IndexLock` | Process-level file lock (RAII) |

### Maintenance Operations

| Operation | Description |
|-----------|-------------|
| `backup/restore` | Index backup and restore |
| `compact` | Merge segments, remove deleted documents |
| `expire` | TTL-based document expiration |
| `rebuild` | Streaming rebuild with schema migration |
