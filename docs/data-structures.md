# 数据结构设计

> Gasket-RS 核心数据结构定义

---

## 1. 消息流转结构

### 1.1 入站消息 (外部 → Agent)

> **来源**: `gasket-types::events::InboundMessage`

```rust
InboundMessage {
    channel: ChannelType,             // 枚举: Telegram | Discord | Slack | Feishu |
                                      //       DingTalk | WeCom | WebSocket | Cli | Custom(String)
    sender_id: String,                // 发送者 ID
    chat_id: String,                  // 对话 ID
    content: String,                  // 消息正文
    media: Option<Vec<MediaAttachment>>,
    metadata: Option<serde_json::Value>,
    timestamp: DateTime<Utc>,
    trace_id: Option<String>,
}
```

### 1.2 出站消息 (Agent → 外部)

> **来源**: `gasket-types::events::OutboundMessage`

```rust
OutboundMessage {
    channel: ChannelType,
    chat_id: String,
    content: String,
    metadata: Option<serde_json::Value>,
    trace_id: Option<String>,
    ws_message: Option<WebSocketMessage>,  // WebSocket 实时消息
}

WebSocketMessage {
    msg_type: WebSocketMessageType,  // Text | Thinking | ToolStart | ToolEnd | TokenStats | Error | Done
    content: String,
    metadata: Option<serde_json::Value>,
}
```
```

### 1.3 会话标识

> **来源**: `gasket-types::events::SessionKey`

```rust
// 强类型会话标识符（替代原来的字符串拼接）
SessionKey {
    channel: ChannelType,     // 渠道类型
    chat_id: String,          // 对话 ID
}
// 序列化格式: "{channel}:{chat_id}"
// 示例: "telegram:12345", "cli:interactive"
```

### 1.4 渠道类型

> **来源**: `gasket-types::events::ChannelType`

```rust
enum ChannelType {
    Telegram,
    Discord,
    Slack,
    Feishu,
    DingTalk,
    WeCom,
    WebSocket,  // WebSocket 实时通信渠道
    Cli,        // 命令行交互
    Custom(String),  // 可扩展的自定义渠道
}
```

### 1.5 媒体附件

> **来源**: `gasket-types::events::MediaAttachment`

```rust
MediaAttachment {
    media_type: String,       // MIME 类型
    url: Option<String>,      // 远程 URL
    data: Option<Vec<u8>>,    // 内联数据
    filename: Option<String>,
}
```

---

## 2. LLM 请求/响应结构

### 2.1 ChatRequest

> **来源**: `gasket-providers::ChatRequest`

```rust
ChatRequest {
    model: String,                        // 如 "deepseek-chat"
    messages: Vec<ChatMessage>,           // 对话历史
    tools: Option<Vec<ToolDefinition>>,   // 可用工具
    temperature: Option<f32>,             // 0.0 ~ 2.0
    max_tokens: Option<u32>,              // 最大生成 token
    thinking: Option<ThinkingConfig>,     // 推理/思考模式
}
```

### 2.2 ChatMessage

> **来源**: `gasket-providers::ChatMessage`
> **注意**: `role` 字段已从 `String` 改为强类型 `MessageRole` 枚举。

```rust
ChatMessage {
    role: MessageRole,                    // 强类型角色枚举
    content: Option<String>,
    tool_calls: Option<Vec<ToolCall>>,    // assistant 发起的工具调用
    tool_call_id: Option<String>,         // tool result 的对应 ID
    name: Option<String>,                 // tool name
}

// 角色类型 (serde 序列化为小写: "system", "user", "assistant", "tool")
enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

// 工厂方法:
ChatMessage::system(content)
ChatMessage::user(content)
ChatMessage::assistant(content)
ChatMessage::assistant_with_tools(content, tool_calls)
ChatMessage::tool_result(id, name, content)
```

### 2.3 ChatResponse

> **来源**: `gasket-providers::ChatResponse`

```rust
ChatResponse {
    content: Option<String>,              // 文本回复
    tool_calls: Vec<ToolCall>,            // 工具调用请求
    reasoning_content: Option<String>,    // 推理/思考内容 (DeepSeek R1 等)
    token_usage: Option<TokenUsage>,      // Token 使用量统计
}

TokenUsage {
    input_tokens: u32,
    output_tokens: u32,
    total_tokens: u32,
}
```
```

### 2.4 ToolCall / ToolDefinition

> **来源**: `gasket-providers::{ToolCall, ToolDefinition}`

```rust
ToolCall {
    id: String,
    r#type: String,           // "function"
    function: FunctionCall {
        name: String,
        arguments: String,    // JSON 字符串
    },
}

ToolDefinition {
    r#type: String,           // "function"
    function: FunctionDefinition {
        name: String,
        description: String,
        parameters: serde_json::Value,  // JSON Schema
    },
}
```

### 2.5 ThinkingConfig

> **来源**: `gasket-providers::ThinkingConfig`

```rust
ThinkingConfig {
    enabled: bool,
    budget_tokens: Option<u32>,  // 推理预算 (token 数)
}
```

### 2.6 流式输出类型

> **来源**: `gasket-types::events::stream`

`StreamEvent` 是对外事件包装器，`ChatEvent` 是内部事件载荷：

```rust
// StreamEvent - 对外事件包装器
pub struct StreamEvent {
    pub agent_id: Option<Arc<str>>,  // None=主代理, Some(uuid)=子代理
    pub event: ChatEvent,           // 事件载荷
}

// ChatEvent - 内部事件载荷枚举
pub enum ChatEvent {
    // 核心事件
    Thinking { content: Arc<str> },
    ToolStart { name: Arc<str>, arguments: Option<Arc<str>> },
    ToolEnd { name: Arc<str>, output: Option<Arc<str>> },
    Content { content: Arc<str> },
    Done,
    Text { content: Arc<str> },
    Error { message: Arc<str> },
    // 上下文统计
    ContextStats { token_budget, compaction_threshold, threshold_tokens, current_tokens, usage_percent, is_compressing },
    WatermarkInfo { watermark, max_sequence, uncompacted_count, compacted_percent },
    // 子代理事件
    SubagentStarted { id, task, index },
    SubagentThinking { id, content },
    SubagentContent { id, content },
    SubagentToolStart { id, name, arguments },
    SubagentToolEnd { id, name, output },
    SubagentCompleted { id, index, summary, tool_count },
    SubagentError { id, index, error },
    SubagentAllStarted { count },
    SubagentSynthesizing {},
    // 审批事件
    ApprovalRequest { id, tool_name, description, arguments },
    ApprovalResponse { ... },
}

// WebSocketMessage 是 ChatEvent 的类型别名
pub type WebSocketMessage = ChatEvent;
```

---

## 3. 事件溯源架构

### 3.1 SessionEvent

表示会话历史中单个事件的不可变事实记录。使用 UUID v7 时间有序标识符进行自然时间排序和数据库友好的索引。

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEvent {
    /// 事件唯一标识符（UUID v7 时间有序）
    pub id: Uuid,

    /// 此事件所属的会话（格式: "channel:chat_id"）
    pub session_key: String,

    /// 事件类型
    pub event_type: EventType,

    /// 消息内容
    pub content: String,

    /// 语义向量（每条消息的嵌入）
    pub embedding: Option<Vec<f32>>,

    /// 事件元数据
    pub metadata: EventMetadata,

    /// 创建时间戳
    pub created_at: DateTime<Utc>,

    /// 单调递增序列号（用于水印压缩和增量查询）
    pub sequence: i64,
}
```

**关键设计点：**
- **UUID v7**：时间有序 UUID 提供自然时间排序，无需时间戳索引
- **sequence**：单调递增序列号，用于水印压缩和 `get_events_after_sequence()` 增量查询
- **嵌入**：可选的语义向量，用于相似性搜索和上下文检索
- **不可变**：事件仅追加，修改创建新事件
- **session_key**：格式为 `"channel:chat_id"`（如 `"telegram:12345"`），同时拆分存储到 `channel` 和 `chat_id` 列

### 3.2 EventType 枚举

表示系统中所有可能事件类型的判别联合。

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EventType {
    /// 用户消息
    UserMessage,

    /// 助手回复
    AssistantMessage,

    /// 工具调用
    ToolCall {
        tool_name: String,
        arguments: serde_json::Value,
    },

    /// 工具结果
    ToolResult {
        tool_call_id: String,
        tool_name: String,
        is_error: bool,
    },

    /// 摘要事件（压缩生成）
    Summary {
        summary_type: SummaryType,
        covered_event_ids: Vec<Uuid>,
    },
}
```

**事件类型类别：**
- **简单变体**：`UserMessage`、`AssistantMessage` - 基本消息类型
- **工具变体**：`ToolCall`、`ToolResult` - 工具执行生命周期
- **元变体**：`Summary` - 历史管理的系统生成事件

### 3.3 SummaryType

指定用于生成摘要事件的策略。

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SummaryType {
    /// 时间窗口摘要
    TimeWindow { duration_hours: u32 },

    /// 主题摘要
    Topic { topic: String },

    /// 压缩摘要（超过 token 预算时）
    Compression { token_budget: usize },
}
```

**摘要策略：**
- **TimeWindow**：汇总特定时间范围内的事件
- **Topic**：汇总与特定主题相关的事件（通过嵌入相似性提取）
- **Compression**：超过 token 预算时触发的激进摘要

### 3.4 EventMetadata

事件的可扩展元数据容器。

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EventMetadata {

    /// 使用的工具列表
    #[serde(default)]
    pub tools_used: Vec<String>,

    /// Token 统计
    pub token_usage: Option<TokenUsage>,

    /// 内容 token 长度（写入时计算，避免读取路径重算）
    #[serde(default)]
    pub content_token_len: usize,

    /// 扩展字段
    #[serde(default)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}
```

**字段：**
- **tools_used**：跟踪在此事件的处理期间调用了哪些工具
- **token_usage**：LLM token 消耗统计，用于成本跟踪
- **content_token_len**：写入时一次性计算的 content token 数，避免读取路径重算
- **extra**：开放式的键值存储，用于未来扩展而无需架构更改

### 3.5 Session（聚合根）

管理会话状态和分支指针的聚合根。

```rust
#[derive(Debug, Clone)]
pub struct Session {
    /// 会话标识符
    pub key: String,

    /// 会话元数据
    pub metadata: SessionMetadata,
}
```

**职责：**
- 提供会话级别的元数据和统计信息

### 3.6 SessionMetadata

会话级别的统计和维护信息。

```rust
#[derive(Debug, Clone, Default)]
pub struct SessionMetadata {
    /// 创建时间戳
    pub created_at: DateTime<Utc>,

    /// 最后更新时间戳
    pub updated_at: DateTime<Utc>,

    /// 最后合并点（事件 ID）
    pub last_consolidated_event: Option<Uuid>,

    /// 总消息数
    pub total_events: usize,

    /// 累计 token 使用量
    pub total_tokens: u64,
}
```

**用途：**
- **last_consolidated_event**：跟踪包含在摘要中的最后一个事件；用于增量摘要
- **total_events/total_tokens**：资源监控和限制的运行计数器

---

## 4. 会话与历史结构（旧版）

### 4.1 Session (Legacy)

> **来源**: `gasket-types::session_event::Session`

```rust
Session {
    key: String,                          // 会话标识 (如 "telegram:12345")
    messages: Vec<SessionMessage>,        // 消息列表
    last_consolidated: usize,             // 上次合并位置
}

SessionMessage {
    role: MessageRole,                    // 强类型角色
    content: String,
    timestamp: DateTime<Utc>,
    tools_used: Option<Vec<String>>,      // 使用的工具列表
}
```

### 4.2 历史处理配置

> **来源**: `gasket_storage::history`

```rust
HistoryConfig {
    max_messages: usize,      // 最大消息条数 (默认 50)
    token_budget: usize,      // token 预算 (默认 4096)
    recent_keep: usize,       // 始终保留最近 N 条 (默认 4)
}

ProcessedHistory {
    messages: Vec<SessionMessage>,        // 保留的消息
    evicted: Vec<SessionMessage>,         // 被驱逐的消息 (用于摘要)
    total_tokens: usize,                  // 总 token 数
}
```

### 4.3 直接存储引用模式（已移除 AgentContext）

组件直接持有 `Arc<EventStore>` / `Arc<SessionStore>`，消除了中间的 `AgentContext` 枚举层。

```rust
// ContextBuilder — 直接持有存储引用
pub struct ContextBuilder {
    event_store: Arc<EventStore>,
    session_store: Arc<SessionStore>,
    // ...
}

// ResponseFinalizer — 直接持有事件存储
pub struct ResponseFinalizer {
    event_store: Arc<EventStore>,
    // ...
}

// AgentSession — 非可选存储字段（AgentSession 本身就是持久化的）
pub struct AgentSession {
    event_store: Arc<EventStore>,
    session_store: Arc<SessionStore>,
    // ...
}
```

**设计优势：**
- 消除间接层 — 组件直接调用存储方法
- 非可选设计 — AgentSession 本身就是持久化会话
- 更简洁的依赖关系图

### 4.4 ContextCompactor

上下文压缩器 — 在 token 预算超限时触发摘要生成。

```rust
pub struct ContextCompactor {
    provider: Arc<dyn LlmProvider>,
    event_store: Arc<EventStore>,
    sqlite_store: Arc<SqliteStore>,
    model: String,
    token_budget: usize,
}

impl ContextCompactor {
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        event_store: Arc<EventStore>,
        sqlite_store: Arc<SqliteStore>,
        model: String,
        token_budget: usize,
    ) -> Self;

    pub fn with_summarization_prompt(self, prompt: impl Into<String>) -> Self;

    /// 非阻塞压缩检查
    pub fn try_compact(
        &self,
        session_key: &SessionKey,
        current_tokens: usize,
    ) -> Option<CompactionResult>;
}
```

**关键设计点：**
- **非阻塞执行**：`try_compact` 生成异步任务，不阻塞响应
- **Token 预算检查**：当 `estimated_tokens` 超过预算时触发压缩
- **后台摘要**：压缩在后台执行，完成后保存到 EventStore

**生命周期：**
```text
AgentSession::process_direct()
  → prepare_pipeline()     // 历史 + 提示组装
  → kernel::execute()      // LLM 迭代 (纯函数)
  → finalize_response()    // 保存事件 + 压缩 + 返回
```

---

## 5. Vault 数据结构

### 5.1 VaultEntryV2

> **来源**: `gasket-engine::vault::VaultEntryV2`

```rust
VaultEntryV2 {
    key: String,                      // 密钥名称
    value: String,                    // 密钥值 (可加密)
    description: Option<String>,      // 描述
    metadata: VaultMetadata,
}

VaultMetadata {
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    last_used: Option<DateTime<Utc>>,
}
```

### 5.2 VaultFileV2

> **来源**: `gasket-engine::vault::VaultFileV2`

```rust
VaultFileV2 {
    version: String,                  // "2.0"
    entries: HashMap<String, VaultEntryV2>,
    encryption: Option<EncryptedData>,
    kdf_params: Option<KdfParams>,    // 密钥派生参数
}
```

### 5.3 InjectionReport

> **来源**: `gasket-engine::vault::injector`

```rust
InjectionReport {
    total_placeholders: usize,        // 占位符总数
    replaced: usize,                  // 成功替换数
    missing_keys: Vec<String>,        // 未找到的密钥
}
```

---

## 6. SQLite 数据库结构

```
~/.gasket/gasket.db  (SqliteStore — sqlx::SqlitePool)
│
│  ─── 会话管理（V2 事件溯源） ───
│
├── sessions_v2              会话元数据（V2）
│   ├── key TEXT PK          会话标识 (如 "cli:interactive", "telegram:12345")
│   ├── channel TEXT         渠道类型 (如 "telegram", "cli")
│   ├── chat_id TEXT         对话 ID
│   ├── created_at TEXT
│   ├── updated_at TEXT
│   ├── last_consolidated_event TEXT
│   ├── total_events INTEGER 事件计数
│   └── total_tokens INTEGER Token 累计
│
├── session_events           事件日志（仅追加，不可变）
│   ├── id TEXT PK           UUID v7 时间有序
│   ├── session_key TEXT     → sessions_v2.key
│   ├── channel TEXT         冗余存储，加速渠道查询
│   ├── chat_id TEXT         冗余存储，加速对话查询
│   ├── event_type TEXT      "user_message" | "assistant_message" | "tool_call" | "tool_result" | "summary"
│   ├── content TEXT         消息内容
│   ├── embedding BLOB       可选 f32 向量
│   ├── tools_used TEXT      JSON 数组
│   ├── token_usage TEXT     JSON TokenUsage
│   ├── token_len INTEGER    内容 token 数（写入时计算）
│   ├── event_data TEXT      工具/摘要类型详情 JSON
│   ├── extra TEXT           扩展 JSON
│   ├── created_at TEXT      ISO 8601
│   └── sequence INTEGER     单调递增序列号（水印压缩用）
│   索引: idx_events_session_sequence ON (session_key, sequence)
│
├── session_summaries        会话摘要检查点
│   ├── session_key TEXT PK
│   ├── content TEXT         摘要内容
│   ├── covered_upto_sequence INTEGER  水印：覆盖到此序列号
│   └── created_at TEXT
│
├── summary_index            摘要事件索引
│   ├── id INTEGER PK AUTOINCREMENT
│   ├── session_key TEXT
│   ├── event_id TEXT        摘要事件的 UUID
│   ├── summary_type TEXT    摘要类型标签
│   ├── topic TEXT           主题摘要的主题
│   ├── covered_events TEXT  覆盖的事件 ID JSON 数组
│   └── created_at TEXT
│
├── session_embeddings       事件嵌入索引
│   ├── message_id TEXT PK
│   ├── session_key TEXT     → sessions_v2.key
│   ├── embedding BLOB       f32 向量
│   └── created_at TEXT
│
│  ─── 通用存储 ───
│
├── kv_store                 键值对
│   ├── key TEXT PK          如 "MEMORY"
│   ├── value TEXT           工作空间文件内容
│   └── updated_at TEXT
│
├── cron_state               定时任务状态（任务定义来自 ~/.gasket/cron/*.md）
│   ├── job_id TEXT PK       任务标识
│   ├── last_run TEXT        上次运行时间
│   └── next_run TEXT        下次运行时间
│
│  ─── 高级搜索 (已迁移到 tantivy-mcp MCP 服务) ───
│
├── (tantivy-mcp 服务)        独立的 MCP 服务器提供全文搜索
```

### 水印压缩设计

事件存储采用 **高水位标记 (High-Water Mark)** 压缩策略：

```
写入路径:
  append_event() → 自动生成 sequence (MAX + 1)
                  → 写入 session_events
                  → 更新 sessions_v2 元数据

读取路径（压缩恢复）:
  1. get_latest_summary() → 获取最新摘要事件
  2. summary.covered_upto_sequence → 水印值
  3. get_events_after_sequence(watermark) → 仅加载水印后的事件
  4. 重组上下文 = 摘要内容 + 增量事件

压缩路径:
  1. get_events_up_to_sequence(target) → 获取待压缩事件
  2. LLM 生成摘要 → 写入新的 Summary 事件
  3. delete_events_upto(target) → 清理已压缩的旧事件
```

---

## 7. 文件系统存储结构

```
~/.gasket/                 工作空间根目录
├── config.yaml             主配置文件
├── gasket.db              SQLite 数据库
├── PROFILE.md              Agent 角色/人格定义
├── SOUL.md                 Agent 灵魂/价值观定义
├── AGENTS.md               Agent 行为/能力描述
├── BOOTSTRAP.md            启动引导信息
├── MEMORY.md               长期记忆 (带 token 硬截断保护)
├── hooks/                  外部 Shell Hook 脚本
│   ├── pre_request.sh      请求预处理
│   └── post_response.sh    响应后处理
├── memory/                 扩展记忆目录
├── skills/                 用户自定义技能
│   └── *.md                Markdown + YAML frontmatter
├── vault/                  敏感数据隔离目录
│   └── secrets.json        加密存储的密钥 (XChaCha20-Poly1305)
```

> **Bootstrap 文件加载顺序**: PROFILE.md → SOUL.md → AGENTS.md → MEMORY.md → BOOTSTRAP.md
>
> MEMORY.md 有 2048 token 硬限制，超出时自动截断保留尾部（最新内容）。

---

## 8. 子代理 (Subagent) 结构

### 8.1 TaskSpec

> **来源**: `gasket-engine::subagents::manager`

子代理任务规格：

```rust
pub struct TaskSpec {
    pub id: String,
    pub task: String,
    pub model: Option<String>,
    pub system_prompt: Option<String>,
}

impl TaskSpec {
    pub fn new(id: impl Into<String>, task: impl Into<String>) -> Self;
    pub fn with_model(mut self, model: impl Into<String>) -> Self;
    pub fn with_system_prompt(mut self, prompt: impl Into<String>) -> Self;
}
```

### 8.2 spawn_subagent

纯函数式子代理创建入口：

```rust
pub fn spawn_subagent(
    provider: Arc<dyn LlmProvider>,
    tools: Arc<ToolRegistry>,
    workspace: PathBuf,
    task: TaskSpec,
    event_tx: Option<mpsc::Sender<StreamEvent>>,
    result_tx: mpsc::Sender<SubagentResult>,
    token_tracker: Option<Arc<TokenTracker>>,
    cancellation_token: CancellationToken,
) -> JoinHandle<()>
```

### 8.3 SubagentResult

> **来源**: `gasket-engine::subagents::tracker`

```rust
pub struct SubagentResult {
    pub id: String,
    pub task: String,
    pub response: AgentResponse,
    pub model: Option<String>,
}
```

> **Note**: There is no separate `SubagentEvent` enum. The unified `StreamEvent` is used for both main agent and subagent events. Subagent events are distinguished by `agent_id: Some(subagent_uuid)`.

---

## 9. Hook 系统结构

### 9.1 Hook 类型定义

> **来源**: `gasket-engine::hooks::types`

```rust
pub enum HookPoint {
    BeforeRequest,    // 请求处理前
    AfterHistory,     // 历史加载后
    BeforeLLM,        // 发送给 LLM 前
    AfterToolCall,    // 工具调用后
    AfterResponse,    // 响应生成后
}

pub enum ExecutionStrategy {
    Sequential,       // 顺序执行，可修改状态
    Parallel,         // 并行执行，只读
}

pub enum HookAction {
    Continue,         // 继续执行
    Abort { reason: String },  // 中止请求
}
```

### 9.2 HookContext

```rust
pub struct HookContext<'a, M> {
    pub session_key: &'a str,
    pub messages: M,
    pub user_input: Option<&'a str>,
    pub response: Option<&'a str>,
    pub tool_calls: Option<&'a [ToolCallInfo]>,
    pub token_usage: Option<&'a TokenUsage>,
    pub vault_values: Vec<String>,
}

pub struct ToolCallInfo {
    pub id: String,
    pub name: String,
    pub arguments: Option<String>,
}

pub type MutableContext<'a> = HookContext<'a, &'a mut Vec<ChatMessage>>;
pub type ReadonlyContext<'a> = HookContext<'a, &'a [ChatMessage>>;
```
