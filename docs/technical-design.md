# Gasket 技术设计文档

> 面向开发人员和架构师的详细技术设计说明

---

## 目录

1. [架构总览](#1-架构总览)
2. [核心抽象层](#2-核心抽象层)
3. [并发模型](#3-并发模型)
4. [数据流详细设计](#4-数据流详细设计)
5. [模块详细设计](#5-模块详细设计)
6. [错误处理策略](#6-错误处理策略)
7. [扩展机制](#7-扩展机制)
8. [性能考量](#8-性能考量)

---

## 1. 架构总览

### 1.1 分层架构

```mermaid
flowchart TB
    subgraph UI["用户接口层"]
        CLI["CLI"]
        TG["Telegram"]
        DC["Discord"]
        WS["WebSocket"]
    end

    subgraph Broker["消息总线层 (Broker)"]
        RA["Router Actor<br/>路由分发"] --> SA["Session Actor<br/>业务处理"] --> OA["Outbound Actor<br/>渠道回发"]
    end

    subgraph Engine["核心引擎层 (Engine)"]
        SESS["Session<br/>状态管理"] --> KERN["Kernel<br/>LLM 循环"] --> TOOL["Tools<br/>工具执行"]
        HOOK["Hooks<br/>生命周期"] ~~~ SKILL["Skills<br/>技能系统"]
    end

    subgraph Infra["基础设施层"]
        PROV["Providers<br/>LLM API"]
        STOR["Storage<br/>SQLite"]
        VAULT["Vault<br/>密钥管理"]
        CRON["Cron<br/>定时任务"]
    end

    UI --> Broker --> Engine --> Infra

    style UI fill:#E3F2FD
    style Broker fill:#FFF3E0
    style Engine fill:#C8E6C9
    style Infra fill:#F3E5F5
```

### 1.2 Crate 组织

| Crate | 职责 | 关键模块 |
|-------|------|----------|
| `gasket-types` | 核心类型定义 | `SessionKey`, `SessionEvent`, `EventType` |
| `gasket-storage` | 持久化层 | `EventStore`, `SqliteStore`, `processor`, `query` |
| `gasket-providers` | LLM  Provider 抽象 | `LlmProvider`, `OpenAICompatibleProvider` |
| `gasket-channels` | 渠道适配器 | `Channel`, `InboundMessage`, `OutboundMessage` |
| `gasket-broker` | 消息总线 | `Router`, `Session`, `Outbound` Actors |
| `gasket-engine` | 核心编排引擎 | `AgentSession`, `Kernel`, `ToolRegistry`, `Hooks` |
| `gasket-vault` | 密钥管理 | `VaultStore`, `VaultInjector` (位于 `engine/src/vault/`) |
| `gasket-cli` | CLI 入口 | 命令行交互、Gateway 启动 |

### 1.3 设计原则

```rust
// 1. 直接存储引用 — 组件持有 Arc<EventStore> / Arc<SessionStore>，消除中间抽象层
pub struct AgentSession {
    event_store: Arc<EventStore>,
    session_store: Arc<SessionStore>,
    // ...
}

// 2. 事件溯源模式
pub struct SessionEvent {
    pub id: Uuid,
    pub session_key: String,
    pub event_type: EventType,  // UserMessage | AssistantMessage | ToolCall | Summary
    pub content: String,
    pub metadata: EventMetadata,
    pub created_at: DateTime<Utc>,
}

// 3. 流式优先设计
// StreamEvent 是对外事件包装器，ChatEvent 是内部事件载荷
pub struct StreamEvent {
    pub agent_id: Option<Arc<str>>,  // None=主代理, Some(uuid)=子代理
    pub event: ChatEvent,            // 事件载荷
}

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
```

---

## 2. 核心抽象层

### 2.1 直接存储引用模式

```rust
// 组件直接持有存储引用，无中间抽象层
// ContextBuilder
pub struct ContextBuilder {
    event_store: Arc<EventStore>,
    session_store: Arc<SessionStore>,
    // ...
}

// ResponseFinalizer
pub struct ResponseFinalizer {
    event_store: Arc<EventStore>,
    // ...
}

// AgentSession — 非可选，AgentSession 本身就是持久化会话
pub struct AgentSession {
    event_store: Arc<EventStore>,
    session_store: Arc<SessionStore>,
    // ...
}
```

**设计决策**: 组件直接持有 `Arc<EventStore>` / `Arc<SessionStore>`，原因：
- 消除间接层，依赖关系更清晰
- 非可选设计 — AgentSession 本身就是持久化会话
- 存储调用内联，无 enum dispatch 开销

### 2.2 LlmProvider: Provider 抽象

```rust
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// 非流式聊天（内部转换为流式实现）
    async fn chat(&self, request: ChatRequest) -> ProviderResult<ChatResponse>;
    
    /// 流式聊天 - 核心接口
    async fn chat_stream(&self, request: ChatRequest) -> ProviderResult<ChatStream>;
    
    /// 健康检查
    async fn health_check(&self) -> ProviderResult<()>;
}

/// OpenAI 兼容 Provider 的通用实现
pub struct OpenAICompatibleProvider {
    client: Client,
    api_key: String,
    base_url: String,
    model_mapping: HashMap<String, String>,
}
```

### 2.3 Tool 系统架构

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> Value;  // JSON Schema
    
    async fn execute(&self, args: Value, ctx: &ToolContext) -> ToolResult;
}

/// 工具执行上下文 - 传递运行时依赖
#[derive(Clone, Default)]
pub struct ToolContext {
    pub spawner: Option<Arc<dyn SubagentSpawner>>,
    pub token_tracker: Option<Arc<TokenTracker>>,
    pub vault_values: Vec<String>,
}

/// 工具注册表 - 内部使用 HashMap 存储
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
}
```

---

## 3. 并发模型

### 3.1 Actor 模型 (Gateway 模式)

```rust
/// Router Actor: 单任务，持有路由表，零锁设计
pub struct RouterActor {
    routes: HashMap<SessionKey, mpsc::Sender<InboundMessage>>,
    session_factory: Arc<dyn SessionFactory>,
}

impl RouterActor {
    pub async fn run(mut self) {
        while let Some(msg) = self.inbound_rx.recv().await {
            let session_key = msg.session_key();
            
            // 懒创建 Session Actor
            if !self.routes.contains_key(&session_key) {
                let (tx, rx) = mpsc::channel(128);
                self.routes.insert(session_key.clone(), tx);
                
                // 启动 Session Actor
                tokio::spawn(
                    SessionActor::new(rx, self.session_factory.clone()).run()
                );
            }
            
            // 转发消息
            if let Some(tx) = self.routes.get(&session_key) {
                let _ = tx.send(msg).await;
            }
        }
    }
}

/// Session Actor: 每 Session 一个，串行处理保证顺序
pub struct SessionActor {
    rx: mpsc::Receiver<InboundMessage>,
    agent_session: Arc<AgentSession>,
}

impl SessionActor {
    async fn run(mut self) {
        while let Some(msg) = self.rx.recv().await {
            // 串行处理 - 保证同一 Session 的消息顺序
            self.process(msg).await;
        }
        // Channel 关闭，Actor 自动清理
    }
}
```

### 3.2 工具并行执行

```rust
/// Kernel 中的工具调用处理
async fn handle_tool_calls(&self, response: &ChatResponse, ...) {
    let futures: Vec<_> = response.tool_calls
        .iter()
        .enumerate()
        .map(|(idx, tc)| async move {
            // 发送 tool_start 事件
            if let Some(ref sender) = tx {
                let _ = sender.send(StreamEvent::tool_start(...)).await;
            }
            
            // 执行工具
            let result = executor.execute_one(&tc, &ctx).await;
            
            // 发送 tool_end 事件
            if let Some(ref sender) = tx {
                let _ = sender.send(StreamEvent::tool_end(...)).await;
            }
            
            (idx, tc.id.clone(), result)
        })
        .collect();
    
    // 等待所有工具完成
    let mut results = futures::future::join_all(futures).await;
    
    // 按原始顺序排序，保证消息顺序一致性
    results.sort_by_key(|(idx, _, _)| *idx);
}
```

### 3.3 流式事件处理

```rust
/// 流式事件累积器 - 处理 LLM 流式输出
pub struct StreamAccumulator;

impl StreamAccumulator {
    pub async fn accumulate_stream(
        stream: ChatStream,
        event_tx: mpsc::Sender<StreamEvent>,
    ) -> Result<ChatResponse, ProviderError> {
        let mut content = String::new();
        let mut reasoning = String::new();
        let mut tool_calls_map: HashMap<usize, ToolCallAccumulator> = HashMap::new();
        
        let mut stream = stream;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            
            // 处理内容增量
            if let Some(delta) = chunk.delta.content {
                content.push_str(&delta);
                let _ = event_tx.send(StreamEvent::Content(delta)).await;
            }
            
            // 处理推理内容（如 DeepSeek-R1）
            if let Some(delta) = chunk.delta.reasoning {
                reasoning.push_str(&delta);
                let _ = event_tx.send(StreamEvent::Reasoning(delta)).await;
            }
            
            // 处理工具调用增量（累积模式）
            if let Some(tool_deltas) = chunk.delta.tool_calls {
                for tool_delta in tool_deltas {
                    let acc = tool_calls_map
                        .entry(tool_delta.index)
                        .or_default();
                    acc.accumulate(&tool_delta);
                }
            }
        }
        
        // 转换为最终响应
        let tool_calls = Self::tool_calls_from_map(tool_calls_map);
        Ok(ChatResponse { content, reasoning_content: Some(reasoning), tool_calls, ... })
    }
}
```

---

## 4. 数据流详细设计

### 4.1 完整请求处理流程

```rust
// Phase 1: 预处理 (Session)
async fn prepare_pipeline(&self, content: &str, session_key: &SessionKey) 
    -> Result<BuildOutcome, AgentError> 
{
    // 1. 执行 BeforeRequest Hooks（可修改/中止）
    let hook_ctx = MutableContext { ... };
    match hooks.execute(HookPoint::BeforeRequest, &mut hook_ctx).await? {
        HookAction::Abort(msg) => return Ok(BuildOutcome::Aborted(msg)),
        HookAction::Continue => { /* modifications applied via mutable ctx */ }
    }
    
    // 2. 保存用户消息到 EventStore
    context.save_event(SessionEvent::user_message(content)).await?;
    
    // 3. 加载历史（Token 感知）
    let history = history::process_history(
        &context, 
        session_key, 
        &HistoryConfig { max_events: config.memory_window, ... }
    ).await?;
    
    // 4. 执行 AfterHistory Hooks
    hooks.execute(HookPoint::AfterHistory, &mut hook_ctx).await?;

    // 5. 组装 Prompt
    let messages = assemble_prompt(
        system_prompt,      // PROFILE.md + SOUL.md + ...
        skills_context,     // 激活的技能
        history.messages,   // 处理后历史
        content,            // 当前输入
    );
    
    Ok(BuildOutcome::Ready(ChatRequest { messages, ... }))
}

// Phase 2: 执行 (Kernel)
async fn execute_streaming(
    runtime_ctx: &RuntimeContext,
    messages: Vec<ChatMessage>,
    event_tx: mpsc::Sender<StreamEvent>,
) -> Result<ExecutionResult, KernelError> {
    let mut state = ExecutionState::new(messages);
    let mut ledger = TokenLedger::new();
    
    for iteration in 1..=config.max_iterations {
        // 1. 发送 LLM 请求（带指数退避重试）
        let stream = request_handler.send_with_retry(request).await?;
        
        // 2. 处理流式响应
        let response = get_response(stream, &event_tx, &mut ledger).await?;
        
        // 3. 检查是否需要工具调用
        if !response.has_tool_calls() {
            return Ok(state.into_result(response.content, ledger));
        }
        
        // 4. 并行执行工具
        handle_tool_calls(&response, &executor, &mut state).await;
    }
    
    Err(KernelError::MaxIterationsReached)
}

// Phase 3: 后处理 (Finalize)
async fn finalize_response(
    result: ExecutionResult,
    ctx: &FinalizeContext,
    event_store: &EventStore,
    hooks: &HookRegistry,
) -> AgentResponse {
    // 1. 脱敏处理后保存到历史
    let safe_content = redact_secrets(&result.content, &ctx.local_vault_values);
    context.save_event(SessionEvent::assistant_message(safe_content)).await?;
    
    // 2. 触发上下文压缩（如需要）
    if ctx.estimated_tokens > config.token_budget {
        compactor.try_compact(&ctx.session_key, ctx.current_tokens);
    }
    
    // 3. 执行 AfterResponse Hooks（并行，只读）
    hooks.execute_parallel(HookPoint::AfterResponse, &readonly_ctx).await;
    
    AgentResponse { ... }
}
```

### 4.2 上下文压缩机制

```rust
/// ContextCompactor: 异步上下文压缩
pub struct ContextCompactor {
    provider: Arc<dyn LlmProvider>,
    event_store: Arc<EventStore>,
    sqlite_store: Arc<SqliteStore>,
    model: String,
    token_budget: usize,
}

impl ContextCompactor {
    /// 非阻塞触发压缩检查
    pub fn try_compact(&self, session_key: &SessionKey, current_tokens: usize) -> Option<CompactionResult> {
        if estimated_tokens < self.token_budget {
            return;
        }
        
        let compactor = self.clone();
        let key = session_key.to_string();
        let vault = vault_values.to_vec();
        
        // 生成摘要（后台执行）
        tokio::spawn(async move {
            match compactor.generate_summary(&key, &vault).await {
                Ok(summary) => {
                    // 保存 Summary 事件
                    let event = SessionEvent::summary(&key, &summary);
                    let _ = compactor.event_store.save(event).await;
                }
                Err(e) => warn!("Compaction failed: {}", e),
            }
        });
    }
    
    async fn generate_summary(&self, session_key: &str, vault_values: &[String]) 
        -> Result<String, CompactorError> 
    {
        // 1. 查询需要压缩的消息
        let events = self.sqlite_store
            .query_events_without_summary(session_key, 50)
            .await?;
        
        // 2. 构建摘要请求
        let prompt = format!("{}", self.summarization_prompt);
        let messages = vec![
            ChatMessage::system(&prompt),
            ChatMessage::user(&format_events(events)),
        ];
        
        // 3. 调用 LLM 生成摘要
        let response = self.provider.chat(ChatRequest { 
            model: self.model.clone(), 
            messages, 
            ... 
        }).await?;
        
        // 4. 标记已压缩的消息
        self.sqlite_store.mark_events_summarized(session_key, &event_ids).await?;
        
        Ok(response.content)
    }
}
```

---

## 5. 模块详细设计

### 5.1 Hook 系统

```rust
/// Hook 执行点
pub enum HookPoint {
    BeforeRequest,   // 请求前 - 顺序执行，可修改/中止
    AfterHistory,    // 历史加载后 - 顺序执行，可修改
    BeforeLLM,       // 发送 LLM 前 - 顺序执行，可修改
    AfterToolCall,   // 工具调用后 - 并行执行，只读
    AfterResponse,   // 响应后 - 并行执行，只读
}

impl HookPoint {
    pub fn execution_strategy(&self) -> ExecutionStrategy {
        match self {
            Self::BeforeRequest | Self::AfterHistory | Self::BeforeLLM 
                => ExecutionStrategy::Sequential,
            Self::AfterToolCall | Self::AfterResponse 
                => ExecutionStrategy::Parallel,
        }
    }
}

/// Hook 上下文 - 可变/只读视图
pub struct MutableContext<'a> {
    pub session_key: &'a str,
    pub messages: &'a mut Vec<ChatMessage>,
    pub user_input: Option<&'a str>,
    pub response: Option<&'a str>,
    pub tool_calls: Option<&'a [ToolCallInfo]>,
    pub token_usage: Option<&'a TokenUsage>,
    pub vault_values: Vec<String>,
}

/// Hook 动作
pub enum HookAction {
    Continue,        // 继续执行
    Modify,          // 已修改上下文，继续
    Abort(String),   // 中止，返回消息
}

#[async_trait]
pub trait PipelineHook: Send + Sync {
    fn hook_point(&self) -> HookPoint;
    async fn execute(&self, ctx: &mut MutableContext) -> Result<HookAction, HookError>;
}
```

### 5.2 Cron 系统

```rust
/// Cron 任务定义（文件存储）
#[derive(Debug, Clone, Deserialize)]
pub struct CronJob {
    pub name: String,
    pub cron: String,           // Cron 表达式
    pub message: String,        // 任务内容
    pub channel: ChannelType,
    pub target: String,         // 用户/群组 ID
    pub enabled: bool,
}

/// Cron 服务
pub struct CronService {
    jobs: Arc<RwLock<Vec<CronJob>>>,
    db: SqlitePool,             // 存储执行状态
    event_tx: mpsc::Sender<InboundMessage>,
}

impl CronService {
    pub async fn run(&self) {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        
        loop {
            interval.tick().await;
            
            let jobs = self.jobs.read().await;
            for job in jobs.iter().filter(|j| j.enabled) {
                if self.should_execute(job).await {
                    // 创建 InboundMessage 注入到 Router
                    let msg = InboundMessage::cron(&job.message, &job.target);
                    let _ = self.event_tx.send(msg).await;
                    
                    // 更新执行状态
                    self.update_execution_time(job).await;
                }
            }
        }
    }
    
    async fn should_execute(&self, job: &CronJob) -> bool {
        let schedule: Schedule = job.cron.parse().unwrap();
        let next = schedule.upcoming(Utc).next().unwrap();
        
        // 检查是否错过执行（如系统关机期间）
        let last_run = self.get_last_execution(&job.name).await;
        Utc::now() >= next || (last_run.is_none() && Utc::now() > next)
    }
}
```

---

## 6. 错误处理策略

### 6.1 错误类型层次

```rust
/// 顶层错误 - 用户可见
#[derive(Error, Debug)]
pub enum AgentError {
    #[error("配置错误: {0}")]
    Config(String),
    
    #[error("Provider 错误: {0}")]
    Provider(#[from] ProviderError),
    
    #[error("工具执行错误: {tool} - {message}")]
    ToolExecution { tool: String, message: String },
    
    #[error("会话错误: {0}")]
    SessionError(String),
    
    #[error("内存错误: {0}")]
    MemoryError(String),
}

/// Provider 错误 - 可重试判断
#[derive(Error, Debug)]
pub enum ProviderError {
    #[error("请求失败: {0}")]
    RequestFailed(String),
    
    #[error("API 错误: {status} - {message}")]
    ApiError { status: u16, message: String },
    
    #[error("流处理错误: {0}")]
    StreamError(String),
    
    #[error("认证失败")]
    AuthenticationFailed,
}

impl ProviderError {
    /// 判断是否可重试
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::RequestFailed(msg) => is_network_error(msg),
            Self::ApiError { status, .. } => matches!(status, 429 | 500 | 502 | 503 | 504),
            _ => false,
        }
    }
}

/// Kernel 内部错误
#[derive(Error, Debug)]
pub enum KernelError {
    #[error("Provider 错误: {0}")]
    Provider(String),
    
    #[error("工具执行错误: {0}")]
    ToolExecution(String),
    
    #[error("达到最大迭代次数")]
    MaxIterationsReached,
    
    #[error("流处理错误: {0}")]
    Stream(String),
}
```

### 6.2 重试策略

```rust
/// 指数退避重试
pub async fn send_with_retry(&self, request: ChatRequest) -> Result<ChatStream> {
    let mut retries = 0u32;
    loop {
        match self.provider.chat_stream(request.clone()).await {
            Ok(stream) => return Ok(stream),
            Err(err) => {
                if !err.is_retryable() || retries >= MAX_RETRIES {
                    return Err(err.into());
                }
                retries += 1;
                
                // 指数退避: 2^retries 秒，最大 15 秒
                let delay = Duration::from_secs(2_u64.pow(retries).min(15));
                tokio::time::sleep(delay).await;
            }
        }
    }
}
```

---

## 7. 扩展机制

### 7.1 自定义 Tool 实现

```rust
use gasket_engine::tools::{Tool, ToolContext, ToolResult, ToolError};

pub struct MyCustomTool {
    config: MyConfig,
}

#[async_trait]
impl Tool for MyCustomTool {
    fn name(&self) -> &str {
        "my_custom_tool"
    }
    
    fn description(&self) -> &str {
        "执行自定义操作"
    }
    
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "input": {
                    "type": "string",
                    "description": "输入参数"
                }
            },
            "required": ["input"]
        })
    }
    
    async fn execute(&self, args: Value, ctx: &ToolContext) -> ToolResult {
        let input = args["input"].as_str()
            .ok_or_else(|| ToolError::InvalidArgs("需要 input 参数".into()))?;
        
        // 实现逻辑
        let result = process(input).await?;
        
        Ok(result.into())
    }
}

// 注册
let mut registry = ToolRegistry::new();
registry.register(Box::new(MyCustomTool::new(config)));
```

### 7.2 自定义 Hook 实现

```rust
use gasket_engine::hooks::{PipelineHook, HookPoint, MutableContext, HookAction};

pub struct AuditHook {
    audit_tx: mpsc::Sender<AuditEvent>,
}

#[async_trait]
impl PipelineHook for AuditHook {
    fn hook_point(&self) -> HookPoint {
        HookPoint::AfterResponse
    }
    
    async fn execute(&self, ctx: &mut MutableContext) -> Result<HookAction, HookError> {
        let event = AuditEvent {
            session_key: ctx.session_key.to_string(),
            timestamp: Utc::now(),
            input: ctx.user_input.map(|s| s.to_string()),
            response: ctx.response.map(|s| s.to_string()),
            token_usage: ctx.token_usage.cloned(),
        };
        
        // 异步发送，不阻塞主流程
        let _ = self.audit_tx.try_send(event);
        
        Ok(HookAction::Continue)
    }
}
```

### 7.3 自定义 Provider 实现

```rust
use gasket_providers::{LlmProvider, ChatRequest, ChatResponse, ChatStream};

pub struct CustomProvider {
    client: Client,
    api_key: String,
}

#[async_trait]
impl LlmProvider for CustomProvider {
    async fn chat(&self, request: ChatRequest) -> ProviderResult<ChatResponse> {
        // 转换为 Provider 特定格式
        let body = self.transform_request(request);
        
        let response = self.client
            .post(&self.api_url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&body)
            .send()
            .await?;
        
        // 转换为标准格式
        self.transform_response(response).await
    }
    
    async fn chat_stream(&self, request: ChatRequest) -> ProviderResult<ChatStream> {
        // SSE 流式实现
        ...
    }
}
```

---

## 8. 性能考量

### 8.1 关键性能指标

| 指标 | 目标值 | 优化策略 |
|------|--------|----------|
| 首 token 延迟 | < 500ms | 连接池复用、预热连接 |
| 流式输出间隔 | < 50ms | 事件驱动、无阻塞处理 |
| Session 启动 | < 100ms | 懒加载、缓存预热 |
| 工具并行执行 | N 个并发 | tokio::spawn + join_all |
| 内存检索 | < 100ms | HNSW 索引、向量缓存 |

### 8.2 内存优化

```rust
/// 1. 使用 Arc 共享大对象
pub struct AgentSession {
    runtime_ctx: RuntimeContext,           // Clone: 增加 Arc 计数
    event_store: Arc<EventStore>,          // Clone: 增加 Arc 计数
    session_store: Arc<SessionStore>,      // Clone: 增加 Arc 计数
    hooks: Arc<HookRegistry>,              // 共享
    compactor: Option<Arc<ContextCompactor>>, // 共享
    indexing_service: Option<Arc<IndexingService>>, // 共享
}

/// 2. 流式处理避免内存拷贝
pub async fn process_stream(
    &self,
    stream: impl Stream<Item = Result<ChatStreamChunk>>,
) -> Result<()> {
    tokio::pin!(stream);
    
    while let Some(chunk) = stream.next().await {
        // 直接转发，不累积
        self.event_tx.send(chunk?).await?;
    }
}

/// 3. 对象池复用
pub struct MessagePool {
    pool: crossbeam::queue::ArrayQueue<ChatMessage>,
}
```

### 8.3 数据库优化

```rust
/// SQLite 连接池配置
pub fn create_pool(db_path: &str) -> Result<SqlitePool> {
    let options = SqliteConnectOptions::new()
        .filename(db_path)
        .journal_mode(SqliteJournalMode::Wal)     // WAL 模式提高并发
        .synchronous(SqliteSynchronous::Normal)   // 平衡性能和耐久
        .pragma("temp_store", "memory")
        .pragma("mmap_size", "30000000000")
        .max_connections(10)
        .min_connections(2)
        .acquire_timeout(Duration::from_secs(30));
    
    SqlitePool::connect_with(options).await
}

/// 关键索引
/// - session_events: (session_key, created_at)
/// - session_events: (event_type)
/// - memory_embeddings: (embedding) - 使用 sqlite-vss 向量索引
```

---

## 附录 A: 类型对照表

| 概念 | Rust 类型 | 存储 |
|------|-----------|------|
| 会话标识 | `SessionKey` | String (channel:user_id:chat_id) |
| 用户消息 | `SessionEvent { event_type: UserMessage }` | SQLite |
| AI 回复 | `SessionEvent { event_type: AssistantMessage }` | SQLite |
| 工具调用 | `SessionEvent { event_type: ToolCall }` | SQLite |
| 上下文摘要 | `SessionEvent { event_type: Summary }` | SQLite |
| 长期记忆 | `Memory { metadata: MemoryMetadata, content: String }` | Markdown + SQLite |
| 技能 | `Skill { metadata: SkillMetadata, content: String }` | 内存 |
| 定时任务 | `CronJob` | Markdown + SQLite |

## 附录 B: 配置优先级

```
1. 环境变量 (GASKET_*)     ← 最高优先级
2. 命令行参数
3. 配置文件 (~/.gasket/config.yaml)
4. 默认值                    ← 最低优先级
```

## 附录 C: 线程安全模型

```mermaid
flowchart TB
    subgraph Runtime["Tokio Runtime"]
        T1["Task 1<br/>Router"]
        T2["Task 2<br/>Session"]
        T3["Task 3<br/>Session"]
        TN["Task N<br/>Session"]
    end

    T1 --> Pool["SQLitePool<br/>连接池线程安全"]
    T2 --> Pool
    T3 --> Pool
    TN --> Pool

    style Runtime fill:#E3F2FD
    style Pool fill:#FFF3E0
```

**数据竞争防护:**
- **Session 状态**: 每个 Session 一个 Task，天然串行
- **Router 状态**: 单 Task，使用 mpsc channel 通信
- **共享数据**: Arc<T> + 内部可变性 (Mutex/RwLock)
