# Gasket 核心模块详细设计

> 各核心模块的架构设计、数据结构和算法详解

---

## 目录

1. [History 模块](#1-history-模块)
2. [Cron 模块](#2-cron-模块)
3. [Kernel 模块](#3-kernel-模块)
4. [Session 模块](#4-session-模块)
5. [Bus 模块](#5-bus-模块)
6. [Types 模块](#6-types-模块)
7. [Storage 模块](#7-storage-模块)
8. [Providers 模块](#8-providers-模块)
9. [CLI 模块](#9-cli-模块)
10. [Sandbox 模块](#10-sandbox-模块)
11. [Subagent 模块](#11-subagent-模块)
12. [Skills 模块](#12-skills-模块)
13. [Vault 模块](#13-vault-模块)
14. [Tools 模块](#14-tools-模块)
15. [Hooks 模块](#15-hooks-模块)
16. [Heartbeat 模块](#16-heartbeat-模块)

---

## 1. History 模块

### 1.1 事件溯源架构

History 模块基于**事件溯源（Event Sourcing）**模式：

```
┌─────────────────────────────────────────────────────────┐
│                     EventStore                           │
│  ┌─────────────────────────────────────────────────────┐│
│  │  SQLite Table: session_events                       ││
│  │  - id: UUID                                          ││
│  │  - session_key: String                               ││
│  │  - event_type: Enum                                  ││
│  │  - content: String                                   ││
│  │  - metadata: JSON                                    ││
│  │  - created_at: DateTime                              ││
│  │  - embedding: Vector(384)                            ││
│  └─────────────────────────────────────────────────────┘│
└─────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────┐
│                 ContextBuilder                           │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────────┐ │
│  │Load Summary │→│Load Events  │→│Process History  │ │
│  │(Watermark)  │  │(After WM)   │  │(Token Budget)   │ │
│  └─────────────┘  └─────────────┘  └─────────────────┘ │
└─────────────────────────────────────────────────────────┘
```

### 1.2 事件类型

```rust
pub enum EventType {
    UserMessage,       // 用户输入
    AssistantMessage,  // AI 回复
    ToolCall {         // 工具调用
        tool_name: String,
        arguments: serde_json::Value,
    },
    ToolResult {       // 工具结果
        tool_call_id: String,
        tool_name: String,
        is_error: bool,
    },
    Summary {          // 上下文摘要
        summary_type: SummaryType,
        covered_event_ids: Vec<Uuid>,
    },
}

pub struct SessionEvent {
    pub id: Uuid,
    pub session_key: String,
    pub event_type: EventType,
    pub content: String,
    pub metadata: EventMetadata,
    pub created_at: DateTime<Utc>,
    pub embedding: Option<Vec<f32>>,  // 用于语义检索
    pub sequence: i64,  // 单调递增序号，用于增量同步和检查点
}

pub struct EventMetadata {
    pub tools_used: Vec<String>,
    pub model: Option<String>,
    pub token_usage: Option<TokenUsage>,
}
```

### 1.3 上下文构建流程

```rust
pub struct ContextBuilder {
    event_store: Arc<EventStore>,
    session_store: Arc<SessionStore>,
    history_config: HistoryConfig,
    memory_loader: Option<MemoryLoader>,
}

impl ContextBuilder {
    pub async fn build(&self, content: &str, session_key: &SessionKey) -> Result<BuildOutcome> {
        // 1. BeforeRequest Hooks
        match self.hooks.execute(HookPoint::BeforeRequest, &mut ctx).await? {
            HookAction::Abort(msg) => return Ok(BuildOutcome::Aborted(msg)),
            _ => {}
        }

        // 2. 加载摘要和水位线
        let (existing_summary, watermark) = self
            .context
            .load_summary_with_watermark(&session_key_str)
            .await;

        // 3. 保存用户事件
        let user_event = SessionEvent::user_message(content);
        self.context.save_event(user_event).await?;

        // 4. 加载水位线之后的事件
        let history_events = self
            .context
            .get_events_after_watermark(&session_key_str, watermark)
            .await;

        // 5. Token 感知的历史处理
        let processed = process_history(history_events, &self.history_config);

        // 6. 组装 Prompt
        let messages = Self::assemble_prompt(
            processed.events,
            &content,
            &system_prompts,
            if existing_summary.is_empty() { None } else { Some(&existing_summary) },
        );

        // 7. AfterHistory + BeforeLLM Hooks
        self.hooks.execute(HookPoint::AfterHistory, &mut ctx).await?;
        self.hooks.execute(HookPoint::BeforeLLM, &mut ctx).await?;

        Ok(BuildOutcome::Ready(ChatRequest { messages, ... }))
    }
}
```

### 1.4 Token 感知历史处理

```rust
pub fn process_history(events: Vec<SessionEvent>, config: &HistoryConfig) -> ProcessedHistory {
    // 1. 按时间排序
    let mut events = events;
    events.sort_by_key(|e| e.created_at);

    // 2. 始终保留最近的 N 条
    let recent_count = events.len().saturating_sub(config.recent_keep);
    let (older, recent) = events.split_at(recent_count);

    // 3. 对较旧的消息应用 Token 预算
    let mut kept = Vec::new();
    let mut total_tokens = recent.iter().map(|e| estimate_tokens(&e.content)).sum::<usize>();

    for event in older.iter().rev() {
        let tokens = estimate_tokens(&event.content);
        if total_tokens + tokens > config.token_budget {
            break;
        }
        total_tokens += tokens;
        kept.push(event.clone());
    }

    // 4. 合并：较旧的消息（倒序）+ 最近的消息（保持顺序）
    kept.reverse();
    kept.extend(recent.iter().cloned());

    ProcessedHistory {
        events: kept,
        evicted: older[..older.len() - kept.len()].to_vec(),
        estimated_tokens: total_tokens,
    }
}
```

### 1.5 摘要机制

```rust
pub struct ContextCompactor {
    provider: Arc<dyn LlmProvider>,
    event_store: Arc<EventStore>,
    sqlite_store: Arc<SqliteStore>,
    model: String,
    token_budget: usize,
}

impl ContextCompactor {
    /// 触发压缩检查（非阻塞）
    pub fn try_compact(&self, session_key: &SessionKey, current_tokens: usize) -> Option<CompactionResult> {
        if estimated_tokens < self.token_budget {
            return;
        }

        let compactor = self.clone();
        let key = session_key.to_string();
        let vault = vault_values.to_vec();

        // 后台执行摘要生成
        tokio::spawn(async move {
            match compactor.generate_summary(&key, &vault).await {
                Ok(summary) => {
                    let event = SessionEvent::summary(&key, &summary);
                    let _ = compactor.event_store.save(event).await;
                }
                Err(e) => warn!("Compaction failed: {}", e),
            }
        });
    }

    async fn generate_summary(&self, session_key: &str, vault_values: &[String]) -> Result<String> {
        // 1. 查询需要压缩的事件（没有 summary_id 的）
        let events = self.sqlite_store
            .query_events_without_summary(session_key, 50)
            .await?;

        // 2. 调用 LLM 生成摘要
        let prompt = self.summarization_prompt.clone();
        let messages = vec![
            ChatMessage::system(&prompt),
            ChatMessage::user(&format_events(events)),
        ];
        let response = self.provider.chat(ChatRequest { messages, ... }).await?;

        // 3. 标记已压缩的事件
        self.sqlite_store.mark_events_summarized(session_key, &event_ids).await?;

        Ok(response.content)
    }
}
```

---

## 2. Cron 模块

### 2.1 混合架构设计

Cron 采用**文件 + 数据库**的混合架构：

```
┌─────────────────────────────────────────────────────────┐
│                      CronService                         │
│                                                          │
│  ┌──────────────────┐      ┌──────────────────────┐    │
│  │  Config Layer    │      │   State Layer        │    │
│  │  ~/.gasket/cron/ │      │   SQLite (cron_state)│    │
│  │  ├── morning.md  │      │   - last_run_at      │    │
│  │  ├── weekly.md   │      │   - next_run_at      │    │
│  │  └── backup.md   │      │   - execution_count  │    │
│  └──────────────────┘      └──────────────────────┘    │
│           │                        │                    │
│           └──────────┬─────────────┘                    │
│                      ▼                                  │
│              ┌───────────────┐                         │
│              │  Memory Cache │  (HashMap<String, CronJob>)│
│              └───────────────┘                         │
└─────────────────────────────────────────────────────────┘
```

### 2.2 核心数据结构

```rust
/// Cron 任务定义
#[derive(Debug, Clone)]
pub struct CronJob {
    pub id: String,                    // 文件名（不含 .md）
    pub name: String,                  // 显示名称
    pub cron: String,                  // Cron 表达式
    pub message: String,               // 任务内容（LLM 处理）
    pub channel: Option<String>,       // 目标渠道
    pub chat_id: Option<String>,      // 目标用户/群组
    pub tool: Option<String>,          // 直接执行的工具（绕过 LLM）
    pub tool_args: Option<Value>,      // 工具参数
    pub last_run: Option<DateTime<Utc>>,
    pub next_run: Option<DateTime<Utc>>,
    pub enabled: bool,
    pub schedule: Option<Schedule>,    // 解析后的 cron 计划
}

/// Markdown Frontmatter 结构
#[derive(Debug, Deserialize)]
struct CronJobFrontmatter {
    name: Option<String>,
    cron: String,
    channel: Option<String>,
    to: Option<String>,           // chat_id 的别名
    enabled: bool,
    tool: Option<String>,
    tool_args: Option<Value>,
}
```

### 2.3 任务执行流程

```rust
impl CronService {
    /// 主循环 - 每分钟检查一次
    pub async fn run(&self) {
        let mut interval = tokio::time::interval(Duration::from_secs(60));

        loop {
            interval.tick().await;

            // 1. 获取到期任务
            let due_jobs = self.get_due_jobs().await?;

            for job in due_jobs {
                // 2. 执行任务
                self.execute_job(&job).await;

                // 3. 推进执行时间戳
                self.advance_job_tick(&job.id).await?;
            }
        }
    }

    /// 获取到期任务
    pub async fn get_due_jobs(&self) -> Result<Vec<CronJob>> {
        let now = Utc::now();
        Ok(self
            .jobs
            .read()
            .values()
            .filter(|job| job.enabled && job.next_run.is_some_and(|nr| nr <= now))
            .cloned()
            .collect())
    }

    /// 推进任务执行时间戳
    pub async fn advance_job_tick(&self, job_id: &str) -> Result<(DateTime<Utc>, DateTime<Utc>)> {
        let now = Utc::now();

        // 1. 更新内存状态
        let next_run = {
            let mut jobs = self.jobs.write();
            let job = jobs.get_mut(job_id).ok_or_else(|| anyhow!("Job not found"))?;

            job.last_run = Some(now);

            // 计算下次执行时间（基于当前时间，处理错过的任务）
            let next = if let Some(schedule) = job.schedule.as_ref() {
                schedule.after(&now).next()
            } else {
                return Err(anyhow!("Job has no valid schedule"));
            };

            job.next_run = Some(next);
            next
        };

        // 2. 持久化到数据库
        self.db
            .upsert_cron_state(job_id, Some(&now.to_rfc3339()), Some(&next_run.to_rfc3339()))
            .await?;

        Ok((now, next_run))
    }
}
```

### 2.4 热重载机制

```rust
impl CronService {
    /// 刷新所有任务（对比 mtime/size）
    pub async fn refresh_all_jobs(&self) -> Result<RefreshReport> {
        let mut report = RefreshReport::default();
        let mut current_ids = HashSet::new();

        for entry in std::fs::read_dir(&cron_dir)? {
            let path = entry.path();
            if path.extension() != Some("md") { continue; }

            // 读取文件元数据
            let metadata = std::fs::metadata(&path)?;
            let disk_mtime = metadata.modified()?;
            let disk_size = metadata.len();

            let job_id = path.file_stem().unwrap().to_str().unwrap().to_string();
            current_ids.insert(job_id.clone());

            // 对比缓存的元数据
            let cached = self.file_metadata.read().get(&job_id).cloned();

            if let Some(cached_meta) = cached {
                if cached_meta.mtime == disk_mtime && cached_meta.size == disk_size {
                    continue; // 无变化，跳过
                }
            }

            // 解析并更新任务
            match self.parse_markdown_file_with_state(&path).await {
                Ok(job) => {
                    self.jobs.write().insert(job_id.clone(), job);
                    self.file_metadata.write().insert(job_id, FileMetadata { mtime: disk_mtime, size: disk_size });
                    report.loaded += 1;
                }
                Err(e) => {
                    report.errors += 1;
                    warn!("Failed to parse cron job: {}", e);
                }
            }
        }

        // 清理已删除的任务
        let existing_ids: Vec<String> = self.jobs.read().keys().cloned().collect();
        for id in existing_ids {
            if !current_ids.contains(&id) {
                self.jobs.write().remove(&id);
                self.file_metadata.write().remove(&id);
                self.db.delete_cron_state(&id).await?;
                report.removed += 1;
            }
        }

        Ok(report)
    }
}
```

### 2.5 系统任务

```rust
impl CronService {
    /// 确保系统维护任务存在
    pub async fn ensure_system_cron_jobs(&self) {
        let system_jobs = [
            (
                "system-cron-refresh",
                "Cron Reload",
                "0 0 * * * * *", // 每小时
                Some("cron".to_string()),
                Some(json!({"action": "refresh"})),
            ),
        ];

        for (id, name, cron_expr, tool, tool_args) in &system_jobs {
            if self.jobs.read().contains_key(*id) { continue; }

            let mut job = CronJob::new(*id, *name, *cron_expr, "system maintenance");
            job.tool = tool.clone();
            job.tool_args = tool_args.clone();

            if let Err(e) = self.add_job(job).await {
                warn!("Failed to create system cron job '{}': {}", id, e);
            }
        }
    }
}
```

---

## 3. Kernel 模块

### 3.1 纯函数设计

Kernel 是**纯函数式**的 LLM 执行核心：

```
┌─────────────────────────────────────────────────────────┐
│                      Kernel                              │
│                                                          │
│   Input: RuntimeContext + Vec<ChatMessage>              │
│                      │                                  │
│                      ▼                                  │
│   ┌──────────────────────────────────────────────┐    │
│   │  AgentExecutor::execute_internal              │    │
│   │  - 最多 20 轮迭代                              │    │
│   │  - 每轮: LLM 请求 → 解析响应 → 执行工具       │    │
│   │  - 工具结果 → 追加到上下文 → 下一轮           │    │
│   └──────────────────────────────────────────────┘    │
│                      │                                  │
│                      ▼                                  │
│   Output: ExecutionResult { content, tools_used, ... }  │
│                                                          │
│   特性:                                                  │
│   - 无副作用（不访问磁盘/网络，除 LLM 和工具外）        │
│   - 相同输入 = 相同输出                                  │
│   - 易于测试和重试                                       │
└─────────────────────────────────────────────────────────┘
```

### 3.2 核心数据结构

```rust
/// 运行时上下文
#[derive(Clone)]
pub struct RuntimeContext {
    pub provider: Arc<dyn LlmProvider>,
    pub tools: Arc<ToolRegistry>,
    pub config: KernelConfig,
    pub spawner: Option<Arc<dyn SubagentSpawner>>,
    pub token_tracker: Option<Arc<TokenTracker>>,
    pub pricing: Option<ModelPricing>,
}

/// Kernel 配置
pub struct KernelConfig {
    pub model: String,
    pub max_iterations: u32,        // 默认 20
    pub temperature: f32,           // 默认 0.7
    pub max_tokens: u32,            // 默认 4096
    pub max_tool_result_chars: usize, // 默认 10000
    pub thinking_enabled: bool,
}

/// 执行结果
#[derive(Debug)]
pub struct ExecutionResult {
    pub content: String,
    pub reasoning_content: Option<String>,
    pub tools_used: Vec<String>,
    pub token_usage: Option<TokenUsage>,
    pub cost: f64,
}
```

### 3.3 执行循环

```rust
pub struct AgentExecutor<'a> {
    provider: Arc<dyn LlmProvider>,
    tools: Arc<ToolRegistry>,
    config: &'a KernelConfig,
    spawner: Option<Arc<dyn SubagentSpawner>>,
}

impl<'a> AgentExecutor<'a> {
    async fn execute_internal(
        &self,
        messages: Vec<ChatMessage>,
        event_tx: Option<mpsc::Sender<StreamEvent>>,
        options: &ExecutorOptions<'_>,
    ) -> Result<ExecutionResult, KernelError> {
        let mut state = ExecutionState::new(messages);
        let mut ledger = TokenLedger::new();
        let executor = ToolExecutor::new(&self.tools, self.config.max_tool_result_chars);
        let request_handler = RequestHandler::new(&self.provider, &self.tools, self.config);

        for iteration in 1..=self.config.max_iterations {
            debug!("[Kernel] iteration {}", iteration);

            let outcome = self
                .process_iteration(
                    iteration,
                    &mut state,
                    &mut ledger,
                    &executor,
                    &request_handler,
                    event_tx.as_ref(),
                    options,
                )
                .await?;

            match outcome {
                IterationOutcome::FinalResponse { content, reasoning_content } => {
                    if let Some(ref tx) = event_tx {
                        let _ = tx.send(StreamEvent::done()).await;
                    }
                    return Ok(state.into_result(content, reasoning_content, ledger));
                }
                IterationOutcome::ContinueWithTools => {}
                IterationOutcome::MaxIterationsReached => {
                    return Ok(state.into_result(DEFAULT_MAX_ITERATIONS.to_string(), None, ledger));
                }
            }
        }

        Ok(state.into_result(DEFAULT_MAX_ITERATIONS.to_string(), None, ledger))
    }
}
```

### 3.4 单轮处理

```rust
async fn process_iteration(
    &self,
    iteration: u32,
    state: &mut ExecutionState,
    ledger: &mut TokenLedger,
    executor: &ToolExecutor<'_>,
    request_handler: &RequestHandler<'_>,
    event_tx: Option<&mpsc::Sender<StreamEvent>>,
    options: &ExecutorOptions<'_>,
) -> Result<IterationOutcome, KernelError> {
    // 1. 构建请求
    let request = request_handler.build_chat_request(&state.messages);

    // 2. 发送请求（带重试）
    let stream_result = request_handler
        .send_with_retry(request)
        .await
        .map_err(|e| KernelError::Provider(e.to_string()))?;

    // 3. 处理流式响应
    let response = self
        .get_response(stream_result, event_tx, ledger, options)
        .await?;

    // 4. 检查是否需要工具调用
    if let Some(outcome) = Self::check_final_response(&response) {
        return Ok(outcome);
    }

    // 5. 并行执行工具
    self.handle_tool_calls(&response, executor, state, event_tx, options).await;

    // 6. 检查最大迭代次数
    if let Some(outcome) = self.check_max_iterations(iteration) {
        return Ok(outcome);
    }

    Ok(IterationOutcome::ContinueWithTools)
}
```

### 3.5 工具并行执行

```rust
async fn handle_tool_calls(
    &self,
    response: &ChatResponse,
    executor: &ToolExecutor<'_>,
    state: &mut ExecutionState,
    event_tx: Option<&mpsc::Sender<StreamEvent>>,
    options: &ExecutorOptions<'_>,
) {
    // 添加 assistant 消息（含 tool_calls）
    state.messages.push(ChatMessage::assistant_with_tools(
        response.content.clone(),
        response.tool_calls.clone(),
    ));

    // 构建工具上下文
    let ctx = ToolContext::default()
        .spawner(self.spawner.clone())
        .token_tracker(options.token_tracker.clone());

    // 并行执行所有工具调用
    let futures: Vec<_> = response
        .tool_calls
        .iter()
        .enumerate()
        .map(|(idx, tc)| {
            let tool_call = tc.clone();
            let ctx = ctx.clone();
            let tx = event_tx.cloned();
            async move {
                // 发送 tool_start 事件
                if let Some(ref sender) = tx {
                    let _ = sender
                        .send(StreamEvent::tool_start(&tool_call.function.name, Some(tool_call.function.arguments.to_string())))
                        .await;
                }

                // 执行工具
                let result = executor.execute_one(&tool_call, &ctx).await;

                // 发送 tool_end 事件
                if let Some(ref sender) = tx {
                    let _ = sender
                        .send(StreamEvent::tool_end(&tool_call.function.name, Some(result.output.clone())))
                        .await;
                }

                (idx, tool_call.id, tool_call.function.name.clone(), result.output)
            }
        })
        .collect();

    // 等待所有工具完成
    let mut results = futures::future::join_all(futures).await;

    // 按原始顺序排序
    results.sort_by_key(|(idx, _, _, _)| *idx);

    // 添加 tool_result 消息
    for (_, tool_call_id, tool_name, output) in results {
        state.tools_used.push(tool_name.clone());
        state.messages.push(ChatMessage::tool_result(tool_call_id, tool_name, output));
    }
}
```

### 3.6 指数退避重试

```rust
impl RequestHandler<'_> {
    pub async fn send_with_retry(&self, request: ChatRequest) -> Result<ChatStream> {
        let mut retries = 0u32;
        loop {
            match self.provider.chat_stream(request.clone()).await {
                Ok(stream) => return Ok(stream),
                Err(provider_err) => {
                    if !provider_err.is_retryable() {
                        return Err(anyhow!("Provider request failed (non-retryable)"));
                    }

                    if retries >= MAX_RETRIES {
                        return Err(anyhow!("Provider request failed after retries"));
                    }
                    retries += 1;

                    // 指数退避: 2^retries 秒，最大 15 秒
                    let delay = Duration::from_secs(2_u64.pow(retries).min(15));
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }
}
```

---

## 4. Session 模块

### 4.1 职责分离

Session 是**有状态**的 orchestration 层，Kernel 是**纯函数**的执行层：

```
┌─────────────────────────────────────────────────────────┐
│                   AgentSession                           │
│  ┌─────────────────────────────────────────────────────┐│
│  │  职责:                                               ││
│  │  1. 生命周期管理（创建/保存/清理）                   ││
│  │  2. 上下文准备（Prompt 组装）                        ││
│  │  3. Hook 执行                                        ││
│  │  4. 后处理（保存回复、触发压缩）                     ││
│  └─────────────────────────────────────────────────────┘│
│                      │                                   │
│                      ▼                                   │
│  ┌─────────────────────────────────────────────────────┐│
│  │                   Kernel                             ││
│  │  职责:                                               ││
│  │  1. LLM 迭代循环                                     ││
│  │  2. 工具调用执行                                     ││
│  │  3. 流式事件处理                                     ││
│  └─────────────────────────────────────────────────────┘│
└─────────────────────────────────────────────────────────┘
```

### 4.2 核心数据结构

```rust
pub struct AgentSession {
    runtime_ctx: RuntimeContext,                              // Kernel 执行上下文
    config: AgentConfig,                                      // Agent 配置
    context_builder: history::builder::ContextBuilder,         // 历史/记忆组装
    compactor: Option<Arc<ContextCompactor>>,            // 上下文压缩器
    pricing: Option<ModelPricing>,                          // 价格配置
    finalizer: ResponseFinalizer,                             // 响应后处理
    pending_done: tokio_util::task::TaskTracker,          // 任务追踪器（无锁）
    pending_asks: Arc<PendingAskRegistryImpl>,            // pending ask 注册表
    #[cfg(feature = "embedding")]
    embedding_indexer: Option<gasket_embedding::EmbeddingIndexer>, // 嵌入索引器
}

pub struct AgentResponse {
    pub content: String,
    pub reasoning_content: Option<String>,
    pub tools_used: Vec<String>,
    pub model: Option<String>,
    pub token_usage: Option<TokenUsage>,
    pub cost: f64,
}
```

### 4.3 请求处理流程

```rust
impl AgentSession {
    /// 流式处理主入口
    pub async fn process_direct_streaming_with_channel(
        &self,
        content: &str,
        session_key: &SessionKey,
    ) -> Result<(mpsc::Receiver<StreamEvent>, tokio::task::JoinHandle<Result<AgentResponse, AgentError>>)> {
        // 1. 预处理管道
        let outcome = self.prepare_pipeline(content, session_key).await?;

        let request = match outcome {
            BuildOutcome::Aborted(msg) => {
                // 返回中止响应
                return Ok((rx, handle));
            }
            BuildOutcome::Ready(req) => req,
        };

        let fctx = FinalizeContext::from_request(&request);
        let messages = request.messages;

        // 2. 启动 Kernel 执行（后台任务）
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(64);

        // 创建完成追踪器
        let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
        self.pending_done.lock().unwrap().push(done_rx);

        let result_handle = tokio::spawn(async move {
            // 执行 Kernel
            let result = kernel::execute_streaming(&runtime_ctx, messages, event_tx).await?;

            // 后处理
            let response = finalize_response(result, &fctx, &context, &hooks, &model, compactor.as_ref()).await;

            // 通知完成
            let _ = done_tx.send(());

            Ok(response)
        });

        Ok((event_rx, result_handle))
    }
}
```

### 4.4 后处理流程

```rust
async fn finalize_response(
    result: ExecutionResult,
    ctx: &FinalizeContext,
    event_store: &EventStore,
    hooks: &HookRegistry,
    model: &str,
    compactor: Option<&Arc<ContextCompactor>>,
) -> AgentResponse {
    // 1. 脱敏处理
    let history_content = redact_secrets(&result.content, &ctx.local_vault_values);

    // 2. 保存 assistant 事件
    let assistant_event = SessionEvent {
        id: uuid::Uuid::now_v7(),
        session_key: ctx.session_key_str.clone(),
        event_type: EventType::AssistantMessage,
        content: history_content,
        metadata: EventMetadata { tools_used: result.tools_used.clone(), ... },
        created_at: chrono::Utc::now(),
        ...
    };
    let _ = context.save_event(assistant_event).await;

    // 3. 触发上下文压缩（非阻塞）
    if ctx.estimated_tokens > 0 {
        if let Some(compactor) = compactor {
            compactor.try_compact(&ctx.session_key, ctx.current_tokens);
        }
    }

    // 4. 执行 AfterResponse Hooks（并行）
    let mut hook_ctx = MutableContext { ... };
    if let Err(e) = hooks.execute(HookPoint::AfterResponse, &mut hook_ctx).await {
        warn!("AfterResponse hook failed (ignored): {}", e);
    }

    AgentResponse { ... }
}
```

### 4.5 优雅关闭

```rust
impl AgentSession {
    /// 优雅关闭，等待所有处理中的任务完成
    pub async fn graceful_shutdown(&self) {
        // 1. 关闭任务追踪器（不再接受新任务）
        self.pending_done.close();

        // 2. 等待所有 pending 的 finalize_response 完成
        if !self.pending_done.is_empty() {
            info!(
                "Graceful shutdown: awaiting {} pending finalization task(s)",
                self.pending_done.len()
            );
        }
        self.pending_done.wait().await;
    }
}
```

---

## 5. Broker 模块

> **注意**: 原 `bus/` crate 已重命名为 `broker/`

### 5.1 Actor 模型架构

Bus 模块实现三 Actor 管道，**零锁设计**：

```
InboundMessage
      │
      ▼
┌─────────────────┐
│  Router Actor   │  (单任务，HashMap 路由表)
│                 │
│  - 按 session_key 分发
│  - 懒创建 Session Actor
│  - 5分钟 GC 死 Session
└───────┬─────────┘
        │ mpsc::channel
        ▼
┌─────────────────┐
│  Session Actor  │  (每 Session 一个)
│                 │
│  - 串行处理消息
│  - 1小时空闲超时
│  - 调用 AgentSession
└───────┬─────────┘
        │ mpsc::channel
        ▼
┌─────────────────┐
│ Outbound Actor  │  (单任务)
│                 │
│  - HTTP 发送
│  - Fire-and-forget
│  - 不阻塞上游
└─────────────────┘
```

### 5.2 核心 Actor 实现

```rust
// ── Router Actor ─────────────────────────────────────────

pub async fn run_router_actor_with_timeout<H>(
    mut inbound_rx: mpsc::Receiver<InboundMessage>,
    outbound_tx: mpsc::Sender<OutboundMessage>,
    handler: Arc<H>,
    idle_timeout: Duration,
) where
    H: MessageHandler + 'static,
{
    let mut sessions: HashMap<SessionKey, mpsc::Sender<InboundMessage>> = HashMap::new();
    let mut cleanup_interval = tokio::time::interval(Duration::from_secs(300));

    loop {
        tokio::select! {
            msg = inbound_rx.recv() => {
                match msg {
                    Some(msg) => {
                        let key = msg.session_key().clone();

                        // 检查是否需要重新创建 Session
                        let mut needs_respawn = true;
                        if let Some(tx) = sessions.get(&key) {
                            if tx.send(msg.clone()).await.is_ok() {
                                needs_respawn = false;
                            } else {
                                info!("Session [{}] channel dead, respawning...", key);
                            }
                        }

                        // 创建新的 Session Actor
                        if needs_respawn {
                            let (tx, rx) = mpsc::channel(32);
                            tokio::spawn(run_session_actor(
                                key.clone(),
                                rx,
                                outbound_tx.clone(),
                                handler.clone(),
                                idle_timeout,
                            ));

                            let _ = tx.send(msg).await;
                            sessions.insert(key, tx);
                        }
                    }
                    None => break,
                }
            }
            _ = cleanup_interval.tick() => {
                // GC 死 Session
                let before = sessions.len();
                sessions.retain(|key, tx| {
                    let alive = !tx.is_closed();
                    if !alive {
                        debug!("Router GC: removing dead session [{}]", key);
                    }
                    alive
                });
                let removed = before - sessions.len();
                if removed > 0 {
                    info!("Router GC: cleaned up {} dead sessions", removed);
                }
            }
        }
    }
}

// ── Session Actor ────────────────────────────────────────

pub async fn run_session_actor<H>(
    session_key: SessionKey,
    mut rx: mpsc::Receiver<InboundMessage>,
    outbound_tx: mpsc::Sender<OutboundMessage>,
    handler: Arc<H>,
    idle_timeout: Duration,
) where
    H: MessageHandler + 'static,
{
    loop {
        let msg = match timeout(idle_timeout, rx.recv()).await {
            Ok(Some(msg)) => msg,
            Ok(None) => {
                info!("Session [{}] channel closed", session_key);
                break;
            }
            Err(_) => {
                info!("Session [{}] idle timeout, GC-ing actor", session_key);
                break;
            }
        };

        // 处理消息
        match process_session_message(msg, &session_key, &handler, &outbound_tx).await {
            Ok(()) => {}
            Err(e) => {
                error!("Session [{}] error: {}", session_key, e);
            }
        }
    }
}

// ── Outbound Actor ──────────────────────────────────────

pub async fn run_outbound_actor(
    mut rx: mpsc::Receiver<OutboundMessage>,
    registry: Arc<OutboundSenderRegistry>,
) {
    while let Some(msg) = rx.recv().await {
        let reg = registry.clone();
        // Fire-and-forget，不阻塞
        tokio::spawn(async move {
            if let Err(e) = reg.send(msg).await {
                error!("Outbound delivery failed: {}", e);
            }
        });
    }
}
```

### 5.3 流式消息处理

```rust
async fn process_streaming_message<H>(
    msg: InboundMessage,
    session_key: &SessionKey,
    handler: &Arc<H>,
    outbound_tx: &mpsc::Sender<OutboundMessage>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    H: MessageHandler + 'static,
{
    // 1. 获取流式事件接收器
    let (mut event_rx, result_handle) = handler
        .handle_streaming_message(&msg.content, session_key)
        .await?;

    // 2. 消费事件（带背压）
    let mut event_count = 0usize;
    while let Some(event) = event_rx.recv().await {
        event_count += 1;

        // 转换为 WebSocket 消息
        if let Some(ws_msg) = stream_event_to_ws_message(event) {
            let outbound_msg = OutboundMessage::with_ws_message(
                msg.channel.clone(),
                msg.chat_id.clone(),
                ws_msg,
            );
            // 使用 .await 提供背压
            outbound_tx.send(outbound_msg).await?;
        }
    }

    // 3. 等待最终结果
    let _response = result_handle.await??;

    info!("Streaming completed: {} events", event_count);
    Ok(())
}

fn stream_event_to_ws_message(event: StreamEvent) -> Option<WebSocketMessage> {
    match event {
        StreamEvent::Content(content) => Some(WebSocketMessage::content(content)),
        StreamEvent::Reasoning(content) => Some(WebSocketMessage::thinking(content)),
        StreamEvent::ToolStart { name, arguments } => {
            Some(WebSocketMessage::tool_start(name, Some(arguments)))
        }
        StreamEvent::ToolEnd { name, output } => {
            Some(WebSocketMessage::tool_end(name, Some(output)))
        }
        StreamEvent::Done => Some(WebSocketMessage::done()),
        StreamEvent::TokenStats { .. } => None,
    }
}
```

---

## 6. Types 模块

### 6.1 架构概述

Types 是整个 Gasket 系统的**基础类型层**，提供跨 Crate 共享的核心类型定义，避免循环依赖：

```
┌─────────────────────────────────────────────────────────────┐
│                      gasket-types                            │
│  ┌──────────────┐ ┌──────────────┐ ┌──────────────────────┐ │
│  │ events.rs    │ │session_event │ │  token_tracker.rs    │ │
│  │ StreamEvent  │ │  EventType   │ │  TokenTracker        │ │
│  │ SessionKey   │ │  SessionEvent│ │  TokenUsage          │ │
│  │ ChannelType  │ │              │ │                      │ │
│  └──────────────┘ └──────────────┘ └──────────────────────┘ │
│  ┌──────────────┐                                          │
│  │ tool.rs      │  ← ToolContext, SubagentSpawner trait    │
│  └──────────────┘                                          │
└─────────────────────────────────────────────────────────────┘
              ↑ 所有其他 Crate 依赖此 Crate
```

**设计决策**: Types 不依赖任何业务 Crate，仅依赖通用库（serde, uuid, chrono, tokio）。

### 6.2 会话标识

```rust
/// 强类型会话标识 (channel:chat_id 格式)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionKey {
    pub channel: ChannelType,
    pub chat_id: String,
}

/// 渠道类型枚举
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub enum ChannelType {
    Telegram,
    Discord,
    Slack,
    Dingtalk,
    Feishu,
    Wecom,
    WebSocket,
    Cli,
    #[default]
    Custom(String),
}
```

### 6.3 事件溯源类型

```rust
/// 不可变事件记录
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEvent {
    pub id: Uuid,                          // UUID v7 时间有序
    pub session_key: String,
    pub event_type: EventType,
    pub content: String,
    pub embedding: Option<Vec<f32>>,       // 语义嵌入向量
    pub metadata: EventMetadata,
    pub created_at: DateTime<Utc>,
    pub sequence: i64,                     // 单调递增序列号
}

/// 事件类型
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EventType {
    UserMessage,
    AssistantMessage,
    ToolCall {
        tool_name: String,
        arguments: serde_json::Value,
    },
    ToolResult {
        tool_call_id: String,
        tool_name: String,
        is_error: bool,
    },
    Summary {
        summary_type: SummaryType,
        covered_event_ids: Vec<Uuid>,
    },
}
```

### 6.4 流式事件类型

```rust
/// 统一流式事件 - 处理所有实时通信
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    // 主 Agent 事件 (agent_id: None)
    Thinking { agent_id: Option<String>, content: String },
    Content { agent_id: Option<String>, content: String },
    ToolStart { agent_id: Option<String>, name: String, arguments: Option<String> },
    ToolEnd { agent_id: Option<String>, name: String, output: Option<String> },
    Done { agent_id: Option<String> },
    TokenStats {
        agent_id: Option<String>,
        input_tokens: usize,
        output_tokens: usize,
        total_tokens: usize,
        cost: f64,
        currency: String,
    },

    // 子 Agent 生命周期事件 (始终有 agent_id)
    SubagentStarted { agent_id: String, task: String, index: u32 },
    SubagentCompleted { agent_id: String, index: u32, summary: String, tool_count: u32 },
    SubagentError { agent_id: String, index: u32, error: String },
}
```

**设计决策**: `agent_id: Option<String>` 字段统一区分主 Agent 和子 Agent 的事件，避免类型爆炸。

### 6.5 Token 追踪

```rust
/// Token 使用量
#[derive(Debug, Clone)]
pub struct TokenUsage {
    pub input_tokens: usize,
    pub output_tokens: usize,
    pub total_tokens: usize,
}

/// 线程安全的 Token 追踪器（原子操作）
#[derive(Debug, Default)]
pub struct TokenTracker {
    total_input_tokens: AtomicUsize,
    total_output_tokens: AtomicUsize,
    total_cost: AtomicU64,           // 以微美分存储，避免浮点精度问题
    budget_limit: AtomicU64,
}
```

### 6.6 工具上下文

```rust
/// 工具执行上下文 - 传递运行时依赖
#[derive(Clone, Default)]
pub struct ToolContext {
    pub session_key: Option<SessionKey>,
    pub outbound_tx: Option<tokio::sync::mpsc::Sender<OutboundMessage>>,
    pub spawner: Option<Arc<dyn SubagentSpawner>>,
    pub token_tracker: Option<Arc<TokenTracker>>,
}

/// 子 Agent 生成器 trait（解耦工具与具体实现）
#[async_trait]
pub trait SubagentSpawner: Send + Sync {
    async fn spawn(
        &self,
        task: String,
        model_id: Option<String>,
    ) -> Result<SubagentResult, Box<dyn std::error::Error + Send>>;
}
```

---

## 7. Storage 模块

### 7.1 架构概述

Storage 是 Gasket 的**持久化层**，采用 SQLite + 文件系统混合架构：

```
┌─────────────────────────────────────────────────────────────────┐
│                        Storage Crate                             │
│                                                                  │
│  ┌──────────────┐ ┌──────────────┐ ┌──────────────────────────┐ │
│  │ SqliteStore  │ │ EventStore   │ │  MetadataStore           │ │
│  │ (连接池管理)  │ │ (事件溯源)   │ │  (记忆元数据)            │ │
│  └──────┬───────┘ └──────┬───────┘ └──────────────────────────┘ │
│         │                │                                        │
│  ┌──────────────┐ ┌──────────────┐ ┌──────────────────────────┐ │
│  │ query.rs     │ │ processor.rs │ │  search/                 │ │
│  │ (多维查询)    │ │ (历史处理)   │ │  (全文检索)              │ │
│  └──────────────┘ └──────────────┘ └──────────────────────────┘ │
└─────────────────────────────────────────────────────────────────┘
```

### 7.2 SQLite Schema

```sql
-- 会话表 (v2 多分支支持)
CREATE TABLE sessions_v2 (
    key TEXT PRIMARY KEY,
    channel TEXT NOT NULL DEFAULT '',
    chat_id TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    last_consolidated_event TEXT,
    total_events INTEGER NOT NULL DEFAULT 0,
    total_tokens INTEGER NOT NULL DEFAULT 0
);

-- 事件溯源表 (核心数据)
CREATE TABLE session_events (
    id TEXT PRIMARY KEY,
    session_key TEXT NOT NULL,
    channel TEXT NOT NULL DEFAULT '',
    chat_id TEXT NOT NULL DEFAULT '',
    event_type TEXT NOT NULL,
    content TEXT NOT NULL,
    embedding BLOB,
    tools_used TEXT DEFAULT '[]',
    token_usage TEXT,
    token_len INTEGER NOT NULL DEFAULT 0,
    event_data TEXT,
    extra TEXT DEFAULT '{}',
    created_at TEXT NOT NULL,
    sequence INTEGER NOT NULL DEFAULT 0
);

-- 摘要表 (带水位线)
CREATE TABLE session_summaries (
    session_key TEXT PRIMARY KEY,
    content TEXT NOT NULL,
    covered_upto_sequence INTEGER NOT NULL DEFAULT 0,  -- 高水位线
    created_at TEXT NOT NULL
);

-- 记忆元数据
CREATE TABLE memory_metadata (
    id TEXT NOT NULL,
    path TEXT NOT NULL,
    scenario TEXT NOT NULL,
    title TEXT NOT NULL DEFAULT '',
    memory_type TEXT NOT NULL DEFAULT 'note',
    frequency TEXT NOT NULL DEFAULT 'warm',
    tags TEXT NOT NULL DEFAULT '[]',
    tokens INTEGER NOT NULL DEFAULT 0,
    updated TEXT NOT NULL DEFAULT '',
    last_accessed TEXT NOT NULL DEFAULT '',
    file_mtime BIGINT NOT NULL DEFAULT 0,
    file_size BIGINT NOT NULL DEFAULT 0,
    needs_embedding INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (scenario, path)
);

-- Cron 执行状态
CREATE TABLE cron_state (
    job_id TEXT PRIMARY KEY,
    last_run_at TEXT,
    next_run_at TEXT
);

-- KV 存储 (机器状态)
CREATE TABLE kv_store (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
```

**关键索引**:

| 索引名 | 字段 | 用途 |
|--------|------|------|
| `idx_events_session_sequence` | `(session_key, sequence)` | 快速会话查询 |
| `idx_events_session_type_created` | `(session_key, event_type, created_at)` | 查找最新摘要 |
| `idx_events_session_sequence` | `(session_key, sequence)` | 水位线 GC |
| `idx_meta_scenario_freq` | `(scenario, frequency)` | 生命周期管理 |

### 7.3 连接池配置

```rust
pub fn create_pool(db_path: &str) -> Result<SqlitePool> {
    let options = SqliteConnectOptions::new()
        .filename(db_path)
        .journal_mode(SqliteJournalMode::Wal)     // WAL 模式提高并发
        .synchronous(SqliteSynchronous::Normal)   // 平衡性能与耐久
        .pragma("temp_store", "memory")
        .pragma("mmap_size", "30000000000")
        .max_connections(5)
        .acquire_timeout(Duration::from_secs(30));

    SqlitePool::connect_with(options).await
}
```

### 7.4 事件查询模式

```rust
/// 多维历史查询
pub struct HistoryQuery {
    pub session_key: String,
    pub time_range: Option<TimeRange>,          // 时间范围过滤
    pub event_types: Vec<String>,               // 事件类型过滤
    pub semantic_query: Option<SemanticQuery>,  // 语义搜索
    pub tools_filter: Vec<String>,              // 工具使用过滤
    pub offset: usize,
    pub limit: usize,
    pub order: QueryOrder,
}

/// 语义查询
pub enum SemanticQuery {
    Text(String),                               // 文本搜索
    Embedding(Vec<f32>),                        // 向量相似度搜索
}
```

### 7.5 全文检索

```rust
/// 搜索查询参数
pub struct SearchQuery {
    pub text: Option<String>,                   // 关键词
    pub boolean: Option<BooleanQuery>,          // AND/OR/NOT 逻辑
    pub fuzzy: Option<FuzzyQuery>,              // 模糊匹配
    pub tags: Vec<String>,                      // 标签过滤
    pub date_range: Option<DateRange>,          // 时间范围
    pub limit: usize,
    pub sort: SortOrder,
    pub role: Option<String>,                   // 消息角色过滤
    pub session_key: Option<String>,            // 会话过滤
}

/// 搜索结果
pub struct SearchResult {
    pub memory: MemoryMeta,
    pub score: f64,                             // 相关性分数
    pub content: String,
    pub highlight: Vec<HighlightedText>,        // 高亮匹配
}
```

---

## 8. Providers 模块

### 8.1 架构概述

Providers 是 LLM API 的**统一抽象层**，通过 Trait 屏蔽各 Provider 差异：

```
┌─────────────────────────────────────────────────────────────────┐
│                      Providers Crate                             │
│                                                                  │
│  ┌─────────────────────────────────────────────────────────┐   │
│  │              LlmProvider Trait (统一接口)                 │   │
│  │  fn chat()      → ChatResponse                          │   │
│  │  fn chat_stream() → ChatStream                          │   │
│  └──────────────────────┬──────────────────────────────────┘   │
│                         │                                       │
│  ┌──────────────────────┼──────────────────────────────────┐   │
│  │                      ▼                                   │   │
│  │  OpenAICompatibleProvider                                │   │
│  │  ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌──────────┐   │   │
│  │  │ OpenAI   │ │DeepSeek  │ │ DashScope│ │OpenRouter│   │   │
│  │  │Anthropic │ │Moonshot  │ │ Zhipu    │ │ MiniMax  │   │   │
│  │  │Ollama    │ │LiteLLM   │ │ Custom   │ │          │   │   │
│  │  └──────────┘ └──────────┘ └──────────┘ └──────────┘   │   │
│  └─────────────────────────────────────────────────────────┘   │
│                                                                  │
│  ┌──────────────┐ ┌──────────────┐ ┌──────────────────────────┐ │
│  │ GeminiProvider│ │CopilotProvider│ │ streaming.rs (SSE 解析) │ │
│  │ (原生 API)    │ │ (OAuth 认证) │ │ model_spec.rs (模型解析) │ │
│  └──────────────┘ └──────────────┘ └──────────────────────────┘ │
└─────────────────────────────────────────────────────────────────┘
```

### 8.2 核心 Trait

```rust
#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn name(&self) -> &str;
    fn default_model(&self) -> &str;

    /// 非流式聊天
    async fn chat(&self, request: ChatRequest)
        -> Result<ChatResponse, ProviderError>;

    /// 流式聊天 - 核心接口
    async fn chat_stream(&self, request: ChatRequest)
        -> Result<ChatStream, ProviderError>;
}

/// 流类型（基于 async Stream）
pub type ChatStream = Pin<Box<
    dyn Stream<Item = Result<ChatStreamChunk, ProviderError>> + Send
>>;
```

### 8.3 请求/响应类型

```rust
/// 聊天请求
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub tools: Option<Vec<ToolDefinition>>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub thinking: Option<ThinkingConfig>,       // 深度推理模式
}

/// 聊天消息
pub struct ChatMessage {
    pub role: MessageRole,                      // System, User, Assistant, Tool
    pub content: Option<String>,
    pub tool_calls: Option<Vec<ToolCall>>,
    pub tool_call_id: Option<String>,
    pub name: Option<String>,
}

/// 聊天响应
pub struct ChatResponse {
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub reasoning_content: Option<String>,      // 推理内容 (DeepSeek-R1 等)
    pub usage: Option<Usage>,
}

/// 流式块
pub struct ChatStreamChunk {
    pub delta: ChatStreamDelta,
    pub finish_reason: Option<FinishReason>,
    pub usage: Option<Usage>,
}
```

### 8.4 OpenAI 兼容 Provider

```rust
pub struct OpenAICompatibleProvider {
    client: Client,
    api_key: String,
    base_url: String,
    model_mapping: HashMap<String, String>,
    extra_headers: HashMap<String, String>,     // 如 MiniMax 的 X-Group-Id
}
```

**支持的 Provider 端点**:

| Provider | Base URL | 特殊特性 |
|----------|----------|----------|
| OpenAI | `api.openai.com/v1` | 标准接口 |
| Anthropic | `api.anthropic.com/v1` | Claude 模型 |
| DeepSeek | `api.deepseek.com/v1` | `reasoning_content` 支持 |
| DashScope | `dashscope.aliyuncs.com/...` | 阿里云 |
| Zhipu | `open.bigmodel.cn/...` | 智谱 |
| MiniMax | `api.minimax.chat/v1` | X-Group-Id 头 |
| OpenRouter | `openrouter.ai/api/v1` | 多模型路由 |
| Ollama | `localhost:11434/v1` | 本地部署 |
| LiteLLM | `localhost:4000/v1` | 代理网关 |

### 8.5 模型规格解析

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSpec {
    pub provider: Option<String>,
    pub model: String,
}

// 解析规则:
// "deepseek/deepseek-chat"      → provider=deepseek, model=deepseek-chat
// "gpt-4o"                       → provider=None, model=gpt-4o
// "openrouter/anthropic/claude-4" → provider=openrouter, model=anthropic/claude-4
```

### 8.6 SSE 流式解析

```rust
/// SSE 流解析器 - 处理 Server-Sent Events
pub fn parse_sse_stream(
    byte_stream: impl Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
) -> impl Stream<Item = Result<ChatStreamChunk, ProviderError>> + Send + 'static {
    // 处理:
    // - 文本增量
    // - 推理内容增量
    // - 工具调用增量（累积模式）
    // - UTF-8 多字节字符跨网络分片
    // - 完成原因和用量统计
}
```

### 8.7 错误处理

```rust
pub enum ProviderError {
    AuthError(String),
    RateLimitError(String),
    InvalidRequest(String),
    ModelNotFound(String),
    NetworkError(String),
    ApiError { status_code: u16, message: String },
    ParseError(String),
    Other(String),
    Internal(Box<dyn std::error::Error + Send + Sync>),
}

impl ProviderError {
    /// 判断是否可重试
    pub fn is_retryable(&self) -> bool { ... }

    /// 获取 HTTP 状态码
    pub fn status_code(&self) -> Option<u16> { ... }
}
```

---

## 9. CLI 模块

### 9.1 架构概述

CLI 是用户与 Gasket 交互的**入口层**，支持单命令和常驻守护两种模式：

```
┌─────────────────────────────────────────────────────────────┐
│                      CLI Crate                               │
│                                                              │
│  main.rs ──→ cli.rs (clap 命令定义)                          │
│                  │                                           │
│  ┌───────────────┼───────────────────────────────────┐      │
│  │               ▼                                    │      │
│  │  ┌──────────┐ ┌──────────┐ ┌──────────────────┐   │      │
│  │  │  Agent   │ │ Gateway  │ │    Onboard       │   │      │
│  │  │ (交互式) │ │(守护进程)│ │   (初始化配置)    │   │      │
│  │  └──────────┘ └──────────┘ └──────────────────┘   │      │
│  │  ┌──────────┐ ┌──────────┐ ┌──────────────────┐   │      │
│  │  │  Status  │ │  Cron    │ │    Memory        │   │      │
│  │  │ (状态)   │ │(定时任务)│ │   (记忆管理)     │   │      │
│  │  └──────────┘ └──────────┘ └──────────────────┘   │      │
│  │  ┌──────────┐ ┌──────────┐ ┌──────────────────┐   │      │
│  │  │  Vault   │ │  Auth    │ │    Channels      │   │      │
│  │  │(密钥)    │ │(认证)    │ │   (渠道管理)      │   │      │
│  │  └──────────┘ └──────────┘ └──────────────────┘   │      │
│  └────────────────────────────────────────────────────┘      │
│                                                              │
│  ┌──────────────────────────────────────────────────────┐   │
│  │  provider.rs (Provider 发现)                          │   │
│  │  registry.rs (工具注册表构建)                          │   │
│  │  interaction/ (用户交互 - 终端审批)                    │   │
│  └──────────────────────────────────────────────────────┘   │
└─────────────────────────────────────────────────────────────┘
```

### 9.2 命令树

```rust
// 顶层命令
enum Commands {
    Onboard,                    // 初始化配置
    Status,                     // 显示系统状态
    Agent(AgentOptions),        // CLI 聊天界面
    Gateway,                    // 启动多渠道网关
    Channels { command },       // 渠道管理
    Auth { command },           // 认证管理
    Cron { command },           // 定时任务
    Stats,                      // Token 使用统计
    Vault { command },          // 密钥管理
    Memory { command },         // 记忆管理（兼容）
}

// Agent 选项
struct AgentOptions {
    -m, --message <STRING>,     // 单消息模式
    --logs,                     // 启用调试日志
    --no-markdown,              // 禁用 Markdown 渲染
    --thinking,                 // 启用推理模式
    --no-stream,                // 禁用流式输出
}
```

### 9.3 Gateway 启动流程

```
                    Gateway 启动序列
                         │
    ┌────────────────────┼────────────────────────┐
    ▼                    ▼                        ▼
配置验证            Provider 初始化          Vault 设置
(config.yaml)      (多 Provider 注册)       (JIT 密钥解析)
    │                    │                        │
    └────────────────────┼────────────────────────┘
                         ▼
              ┌─────────────────────┐
              │   MessageBus 创建   │
              │  (buffer: 512)      │
              └──────────┬──────────┘
                         │
    ┌────────────────────┼────────────────────┐
    ▼                    ▼                    ▼
HeartbeatService    CronService         渠道初始化
(定时任务检查)      (60秒轮询)         (按 Feature 编译)
    │                    │                    │
    └────────────────────┼────────────────────┘
                         ▼
              ┌─────────────────────┐
              │   Actor 管道启动    │
              │  Outbound → Router  │
              │  → Session Actors   │
              └──────────┬──────────┘
                         │
                         ▼
              ┌─────────────────────┐
              │  信号等待 (Ctrl+C)  │
              │  优雅关闭           │
              └─────────────────────┘
```

### 9.4 Provider 发现

```rust
/// 默认 Provider 解析顺序
const DEFAULT_PROVIDER_ORDER: &[&str] = &[
    "openrouter", "deepseek", "openai", "anthropic",
    "litellm", "ollama",
];

/// 模型解析器
// 支持:
// 1. 命名配置: "smart-assistant" → 查找 models 配置
// 2. Provider/Model: "minimax/abab6.5-chat" → 直接解析
// 3. 裸 Provider: "minimax" → 使用默认模型
```

### 9.5 工具注册表构建

```rust
pub struct ToolRegistryConfig {
    pub config: Config,
    pub workspace: PathBuf,
    pub subagent_spawner: Option<Arc<dyn SubagentSpawner>>,
    pub extra_tools: Vec<(Box<dyn Tool>, ToolMetadata)>,
    pub sqlite_store: Option<SqliteStore>,
    pub model_registry: Option<Arc<ModelRegistry>>,
    pub provider_registry: Option<Arc<ProviderRegistry>>,
}

// 工具分类:
// - 安全只读: ReadFileTool, ListDirTool, WebFetchTool, WebSearchTool
// - 危险可变: WriteFileTool, EditFileTool, ExecTool (需审批)
// - 网关专用: MessageTool, CronTool, SpawnTool, SpawnParallelTool
```

---

## 10. Sandbox 模块

### 10.1 架构概述

Sandbox 是 Gasket 的**代码执行安全层**，采用纵深防御策略：

```
┌─────────────────────────────────────────────────────────────────┐
│                      Sandbox Crate                               │
│                                                                  │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │                  Defense-in-Depth 纵深防御                 │  │
│  │                                                           │  │
│  │  Layer 1: Command Policy (命令策略 - 咨询性过滤)          │  │
│  │  ┌─────────────────────────────────────────────────────┐│  │
│  │  │  Allowlist + Denylist 检查                            ││  │
│  │  └──────────────────────────┬──────────────────────────┘│  │
│  │                             ▼                            │  │
│  │  Layer 2: Sandbox Isolation (沙箱隔离 - 真实安全边界)    │  │
│  │  ┌─────────┐ ┌───────────┐ ┌──────────┐ ┌───────────┐ │  │
│  │  │  bwrap  │ │sandbox-exec│ │ Windows  │ │ Fallback  │ │  │
│  │  │ (Linux) │ │  (macOS)  │ │(unsafe)  │ │  (sh/ulimit)│ │  │
│  │  └─────────┘ └───────────┘ └──────────┘ └───────────┘ │  │
│  │                             ▼                            │  │
│  │  Layer 3: Resource Limits (资源限制)                     │  │
│  │  ┌─────────────────────────────────────────────────────┐│  │
│  │  │  内存/CPU/输出/进程数/文件大小                        ││  │
│  │  └──────────────────────────┬──────────────────────────┘│  │
│  │                             ▼                            │  │
│  │  Layer 4: Approval System (审批系统 - 人工监督)          │  │
│  │  ┌─────────────────────────────────────────────────────┐│  │
│  │  │  Denied / AskAlways / AskOnce / Allowed              ││  │
│  │  └─────────────────────────────────────────────────────┘│  │
│  └──────────────────────────────────────────────────────────┘  │
│                                                                  │
│  ┌──────────────┐ ┌──────────────┐ ┌──────────────────────────┐ │
│  │  Audit Log   │ │ Interaction  │ │  Process Manager         │ │
│  │  (审计日志)   │ │ (用户交互)   │ │  (进程管理)              │ │
│  └──────────────┘ └──────────────┘ └──────────────────────────┘ │
└─────────────────────────────────────────────────────────────────┘
```

### 10.2 核心配置

```rust
/// 沙箱配置
pub struct SandboxConfig {
    pub enabled: bool,
    pub backend: String,                    // auto | fallback | bwrap | sandbox-exec
    pub tmp_size_mb: u32,                   // tmpfs 大小
    pub workspace: Option<PathBuf>,
    pub limits: ResourceLimitsConfig,
    pub policy: CommandPolicyConfig,
    pub approval: ApprovalConfig,
    pub audit: AuditConfig,
}

/// 资源限制
pub struct ResourceLimits {
    pub max_memory_bytes: u64,
    pub max_cpu_secs: u32,
    pub max_output_bytes: usize,
    pub max_processes: u32,
    pub max_file_size_bytes: u64,
    pub max_open_files: u32,
}

/// 命令策略
pub struct CommandPolicy {
    allowlist: Vec<String>,
    denylist: Vec<String>,
}
```

### 10.3 审批系统

```rust
/// 权限级别
pub enum PermissionLevel {
    Denied,         // 永久拒绝
    AskAlways,      // 每次询问
    AskOnce,        // 会话内询问一次
    Allowed,        // 永久允许
}

/// 操作类型
pub enum OperationType {
    Command { binary: String, args: Option<String> },
    FileRead { path_pattern: String },
    FileWrite { path_pattern: String },
    Network { host_pattern: String, port: Option<u16> },
    EnvVar { name_pattern: String },
    Custom { category: String, name: String },
}
```

### 10.4 平台实现

#### Linux (bwrap)

```rust
// 使用 bubblewrap 进行命名空间隔离
command.arg("--unshare-pid").arg("--unshare-ipc");
// 只读根文件系统
command.arg("--ro-bind").arg("/").arg("/");
// 可写工作区
command.arg("--bind").arg(working_dir).arg(working_dir);
// 受限 tmpfs
command.arg("--tmpfs").arg("/tmp");
```

**挂载布局**:
- `/` → 只读绑定自宿主
- `workspace` → 读写绑定
- `/tmp` → 限定大小的 tmpfs
- `/dev` → 最小 devtmpfs
- `/proc` → 新 proc 命名空间

#### macOS (sandbox-exec)

```rust
// 使用 Apple Seatbelt 沙箱
// 自定义 Profile: 允许所有读操作，限制写操作到 workspace/tmp
let profile = r#"
(deny default)
(allow file-read*)
(allow file-write* (subpath "workspace") (subpath "/tmp"))
(allow process-exec)
"#;
```

#### Fallback (通用)

```rust
// sh -c + ulimit 资源限制
// 无真正隔离，仅资源限制
// 适用于沙箱禁用或不可用的场景
```

### 10.5 执行结果

```rust
pub struct ExecutionResult {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub resource_exceeded: bool,            // 是否超出资源限制
    pub duration_ms: u64,
}
```

---

## 11. Subagent 模块

### 11.1 架构概述

Subagent 是 Gasket 的**多 Agent 协作系统**，支持将任务委托给独立子 Agent 执行：

```
┌─────────────────────────────────────────────────────────────────┐
│                    Subagent Architecture                         │
│                                                                  │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │                   SimpleSpawner                           │  │
│  │  provider: Arc<dyn LlmProvider>                           │  │
│  │  tools: Arc<ToolRegistry>                                 │  │
│  │  model_resolver: Option<Arc<dyn ModelResolver>>           │  │
│  └──────────────────────┬───────────────────────────────────┘  │
│                         │                                       │
│           ┌─────────────┼─────────────┐                        │
│           ▼             ▼             ▼                        │
│  ┌──────────────┐ ┌──────────┐ ┌──────────────┐               │
│  │ spawn_subagent│ │ SpawnTool│ │SpawnParallel │               │
│  │ (纯函数)      │ │ (单任务) │ │  Tool(并行)  │               │
│  └──────┬───────┘ └──────────┘ └──────┬───────┘               │
│         │                             │                        │
│         ▼                             ▼                        │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │              SubagentTracker (并行协调)                    │  │
│  │  - 结果收集 (RwLock<HashMap>)                              │  │
│  │  - 事件转发 (统一 StreamEvent)                            │  │
│  │  - 取消支持 (CancellationToken)                           │  │
│  │  - 并发限制 (Semaphore: 5)                                │  │
│  └──────────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────┘
```

### 11.2 核心数据结构

```rust
/// 任务规格
pub struct TaskSpec {
    pub id: String,
    pub task: String,
    pub model: Option<String>,               // 可选模型配置
    pub system_prompt: Option<String>,
}

/// 子 Agent 生成器
pub struct SimpleSpawner {
    provider: Arc<dyn LlmProvider>,
    tools: Arc<ToolRegistry>,
    workspace: PathBuf,
    token_tracker: Option<Arc<TokenTracker>>,
    model_resolver: Option<Arc<dyn ModelResolver>>,
}

/// 子 Agent 结果
pub struct SubagentResult {
    pub id: String,
    pub task: String,
    pub response: AgentResponse,
    pub model: Option<String>,
}
```

### 11.3 生成流程

```rust
/// spawn_subagent - 纯函数，最小开销
async fn spawn_subagent(
    spec: TaskSpec,
    provider: Arc<dyn LlmProvider>,
    tools: Arc<ToolRegistry>,
    workspace: PathBuf,
    event_tx: Option<mpsc::Sender<StreamEvent>>,
    token_tracker: Option<Arc<TokenTracker>>,
) -> Result<SubagentResult> {
    // 1. 创建 tokio 任务
    // 2. 加载系统提示 (回退到 BOOTSTRAP_FILES_MINIMAL)
    // 3. 构建 Kernel 上下文 (provider, tools, config)
    // 4. 执行，10 分钟超时 (SUBAGENT_TIMEOUT_SECS = 600)
    // 5. 转发流式事件（注入 agent_id）
    // 6. 累积 Token 用量
}
```

### 11.4 并行协调

```rust
/// 子 Agent 追踪器
pub struct SubagentTracker {
    results: Arc<RwLock<HashMap<String, SubagentResult>>>,
    result_tx: mpsc::Sender<SubagentResult>,
    result_rx: Option<mpsc::Receiver<SubagentResult>>,   // 所有权管理
    event_tx: mpsc::Sender<StreamEvent>,
    event_rx: Option<mpsc::Receiver<StreamEvent>>,
    cancellation_token: CancellationToken,                // 取消支持
}

// 并发限制: Semaphore(5) 防止 API 速率限制
// 事件区分: agent_id 字段区分主 Agent 和子 Agent
```

### 11.5 模型解析

```rust
/// 模型解析器 trait
pub trait ModelResolver: Send + Sync {
    fn resolve_model(&self, model_id: &str)
        -> Option<(Arc<dyn LlmProvider>, AgentConfig)>;
}

// 解析策略:
// 1. 命名配置: "coder" → 查找 models.coder 配置
// 2. Provider/Model: "deepseek/deepseek-coder" → 直接解析
// 3. 裸 Provider: "deepseek" → 使用默认模型

// 配置示例:
// models:
//   coder:
//     provider: "deepseek"
//     model: "deepseek-coder"
//     temperature: 0.3
//     thinking_enabled: true
```

---

## 12. Skills 模块

### 12.1 架构概述

Skills 是 Gasket 的**技能系统**，支持动态加载和语义路由：

```
┌─────────────────────────────────────────────────────────────────┐
│                      Skills System                               │
│                                                                  │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │                  SkillsLoader                              │  │
│  │  目录扫描:                                                 │  │
│  │  ├── Flat: skills/weather.md                              │  │
│  │  └── Nested: skills/weather/SKILL.md                      │  │
│  └──────────────────────┬───────────────────────────────────┘  │
│                         │                                       │
│                         ▼                                       │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │                 SkillsRegistry                             │  │
│  │  skills: HashMap<String, Skill>                           │  │
│  │  embeddings: OnceLock<Vec<(String, Vec<f32>)>>            │  │
│  │                                                           │  │
│  │  方法:                                                    │  │
│  │  - register() → 注册技能                                  │  │
│  │  - get_top_k() → 语义 Top-K 检索                         │  │
│  │  - generate_context_summary() → 生成上下文摘要            │  │
│  │  - list_available() → 列出可用技能                        │  │
│  └──────────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────┘
```

### 12.2 核心数据结构

```rust
/// 技能注册表
pub struct SkillsRegistry {
    skills: HashMap<String, Skill>,
    loader: Option<SkillsLoader>,
    embeddings: OnceLock<Vec<(String, Vec<f32>)>>,    // 延迟计算的嵌入缓存
}

/// 技能定义
pub struct Skill {
    metadata: SkillMetadata,
    content: String,                  // 空 = 懒加载
    path: PathBuf,
    available: bool,                  // 依赖是否满足
    missing_deps: Vec<String>,
}

/// 技能元数据 (YAML Frontmatter)
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub always: bool,                 // true = 始终注入上下文
    pub bins: Vec<String>,            // 依赖的二进制文件
    pub env_vars: Vec<String>,        // 依赖的环境变量
}
```

### 12.3 加载策略

```rust
impl SkillsLoader {
    /// 两种目录布局
    /// Flat:    ~/.gasket/skills/weather.md
    /// Nested:  ~/.gasket/skills/weather/SKILL.md
    async fn load_from_directory(&self, dir: &Path) -> Result<Vec<Skill>> {
        // 1. 扫描目录
        // 2. 解析 YAML Frontmatter
        // 3. 检查依赖 (bins, env_vars)
        // 4. 标记 available
    }
}

impl SkillsRegistry {
    /// 语义 Top-K 检索
    pub fn get_top_k(&self, query: &str, k: usize) -> Vec<&Skill> {
        // 1. 首次调用时计算所有技能嵌入
        // 2. 计算查询嵌入
        // 3. 余弦相似度排序
        // 4. 返回 Top-K
    }

    /// 生成上下文摘要
    pub fn generate_context_summary(&self, query: &str, budget: usize) -> String {
        // 1. always=true 的技能直接注入
        // 2. 剩余预算用 Top-K 填充
    }
}
```

### 12.4 技能文件格式

```markdown
---
name: weather-query
description: 查询天气信息
always: false
bins:
  - curl
env_vars:
  - WEATHER_API_KEY
---

# Weather Query Skill

You can query weather information by...
```

---

## 13. Vault 模块

### 13.1 架构概述

Vault 是 Gasket 的**密钥管理系统**，提供加密存储和运行时注入：

```
┌─────────────────────────────────────────────────────────────────┐
│                      Vault System                                │
│                                                                  │
│  ┌──────────────────────┐    ┌──────────────────────────────┐  │
│  │    VaultStore        │    │      VaultInjector            │  │
│  │ (加密存储)            │    │ (运行时注入)                  │  │
│  │                      │    │                               │  │
│  │ PBKDF2 + AES-256-GCM │    │ {{vault:key}} → 实际值       │  │
│  │ 内存或文件存储        │    │ InjectionReport 注入报告      │  │
│  └──────────────────────┘    └──────────────────────────────┘  │
│                                                                  │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │  Placeholder Scanner                                     │  │
│  │  - scan_placeholders()  检测所有密钥占位符               │  │
│  │  - replace_placeholders() 执行实际替换                   │  │
│  │  - redact() 脱敏（安全日志）                              │  │
│  └──────────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────┘
```

### 13.2 核心数据结构

```rust
/// 密钥存储
pub struct VaultStore {
    entries: HashMap<String, VaultEntryV2>,
    metadata: VaultMetadata,
    locked: bool,
}

/// 密钥条目
pub struct VaultEntryV2 {
    pub name: String,
    pub ciphertext: EncryptedData,           // PBKDF2 + AES-256-GCM 加密
    pub description: Option<String>,
    pub created_at: AtomicTimestamp,         // 原子时间戳
    pub last_used: AtomicTimestamp,
}

/// 注入器
pub struct VaultInjector {
    store: Arc<VaultStore>,
}

/// 注入报告
pub struct InjectionReport {
    pub messages_modified: usize,
    pub keys_used: Vec<String>,
    pub missing_keys: Vec<String>,           // 缺失的密钥（警告）
    pub injected_values: Vec<String>,        // 注入的值（用于后处理脱敏）
}
```

### 13.3 运行时注入流程

```rust
impl VaultInjector {
    /// 注入密钥到消息列表
    pub fn inject(&self, messages: &mut Vec<ChatMessage>) -> InjectionReport {
        let mut report = InjectionReport::default();

        for message in messages.iter_mut() {
            if let Some(ref mut content) = message.content {
                // 1. 扫描 {{vault:key}} 占位符
                let placeholders = scan_placeholders(content);

                for key in placeholders {
                    match self.store.get(&key) {
                        Ok(value) => {
                            // 2. 替换占位符
                            *content = content.replace(&format!("{{{{vault:{}}}}}", key), &value);
                            report.keys_used.push(key);
                            report.injected_values.push(value);
                        }
                        Err(_) => {
                            report.missing_keys.push(key);
                        }
                    }
                }
            }
        }

        report.messages_modified = report.keys_used.len();
        report
    }
}
```

### 13.4 配置集成

```yaml
# config.yaml 中的使用
providers:
  openrouter:
    apiKey: "{{vault:openrouter_token}}"

channels:
  telegram:
    token: "{{vault:telegram_bot_token}}"
```

**设计决策**: JIT（Just-In-Time）解析策略——配置文件中只存储占位符，运行时才替换为真实值，确保密钥不落盘。

---

## 14. Tools 模块

### 14.1 架构概述

Tools 是 Gasket 的**工具执行系统**，支持内置工具和 MCP 外部工具：

```
┌─────────────────────────────────────────────────────────────────┐
│                      Tools System                                │
│                                                                  │
│  ┌──────────────────────────────────────────────────────────┐  │
│  │                  ToolRegistry                             │  │
│  │  items: HashMap<String, RegisteredTool>                   │  │
│  │  embeddings: OnceLock<Vec<(String, Vec<f32>)>>            │  │
│  │                                                           │  │
│  │  - register() → 注册工具                                  │  │
│  │  - get() → 按名称查找                                    │  │
│  │  - get_top_k() → 语义 Top-K 工具检索                     │  │
│  │  - tool_definitions() → 生成 LLM 工具定义列表            │  │
│  └──────────────────────┬───────────────────────────────────┘  │
│                         │                                       │
│  ┌──────────────────────┼──────────────────────────────────┐  │
│  │                      ▼                                   │  │
│  │  内置工具                                                │  │
│  │  ┌───────────┐ ┌───────────┐ ┌───────────────────────┐ │  │
│  │  │ ExecTool  │ │FS Tools   │ │ Web Tools             │ │  │
│  │  │(沙箱执行) │ │Read/Write │ │Fetch/Search           │ │  │
│  │  │           │ │Edit/List  │ │                       │ │  │
│  │  └───────────┘ └───────────┘ └───────────────────────┘ │  │
│  │  ┌───────────┐ ┌───────────────────────┐               │  │
│  │  │Agent Tools│ │   MCP Tools           │               │  │
│  │  │Spawn/Cron │ │(JSON-RPC 2.0 over     │               │  │
│  │  │Message   │ │ stdio)                │               │  │
│  │  └───────────┘ └───────────────────────┘               │  │
│  └─────────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────────┘
```

### 14.2 核心 Trait

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> serde_json::Value;        // JSON Schema
    async fn execute(&self, args: Value, ctx: &ToolContext) -> ToolResult;
}
```

### 14.3 ExecTool 安全模型

```rust
pub struct ExecTool {
    working_dir: PathBuf,
    timeout: Duration,
    restrict_to_workspace: bool,
    enabled: bool,
    process_manager: ProcessManager,
}

// 纵深防御:
// 1. 命令策略检查 (Allowlist/Denylist)
// 2. 危险模式阻断: ;, &&, ||, `, $(
// 3. 沙箱隔离 (bwrap/sandbox-exec)
// 4. 资源限制 (内存/CPU/输出)
// 5. 审批系统 (AskAlways/AskOnce/Allowed)
```

### 14.4 工具注册

```rust
pub struct RegisteredTool {
    tool: Box<dyn Tool>,
    metadata: ToolMetadata,
}

pub struct ToolMetadata {
    pub category: String,           // fs, web, agent, mcp
    pub requires_approval: bool,    // 是否需要用户审批
    pub dangerous: bool,            // 是否标记为危险操作
}

impl ToolRegistry {
    /// 生成 LLM 工具定义
    pub fn tool_definitions(&self) -> Vec<ToolDefinition> {
        self.items.values().map(|rt| ToolDefinition {
            name: rt.tool.name().to_string(),
            description: rt.tool.description().to_string(),
            parameters: rt.tool.parameters(),
        }).collect()
    }
}
```

### 14.5 MCP 集成

```rust
/// MCP 工具通过 JSON-RPC 2.0 over stdio 通信
pub struct McpTool {
    name: String,
    description: String,
    parameters: Value,
    client: McpClient,              // JSON-RPC 客户端
}

impl McpTool {
    /// 通过 stdio 调用外部工具服务器
    async fn execute(&self, args: Value, _ctx: &ToolContext) -> ToolResult {
        let request = jsonrpc::Request {
            method: format!("tools/{}", self.name),
            params: Some(args),
            ..
        };
        self.client.call(request).await
    }
}
```

---

## 15. Hooks 模块

### 15.1 架构概述

Hooks 是 Gasket 的**生命周期钩子系统**，支持在请求处理管道的各阶段插入自定义逻辑：

```
┌─────────────────────────────────────────────────────────────────┐
│                  Hook 执行点与策略                                │
│                                                                  │
│  请求 → [BeforeRequest] → [AfterHistory] → [BeforeLLM]         │
│           顺序执行              顺序执行        顺序执行          │
│           可修改/中止           可修改          可修改/中止       │
│                │                  │                │             │
│                ▼                  ▼                ▼             │
│            ┌──────────────────────────────────────────┐         │
│            │         LLM 执行 + 工具调用               │         │
│            └──────────────────┬───────────────────────┘         │
│                               │                                  │
│                ┌──────────────┼──────────────┐                  │
│                ▼                             ▼                  │
│          [AfterToolCall]              [AfterResponse]           │
│            并行执行                     并行执行                  │
│            只读                         只读                     │
└─────────────────────────────────────────────────────────────────┘
```

### 15.2 核心 Trait 和类型

```rust
/// Hook 执行点
pub enum HookPoint {
    BeforeRequest,   // 请求前 - 顺序执行，可修改/中止
    AfterHistory,    // 历史加载后 - 顺序执行，可修改
    BeforeLLM,       // 发送 LLM 前 - 顺序执行，可修改
    AfterToolCall,   // 工具调用后 - 并行执行，只读
    AfterResponse,   // 响应后 - 并行执行，只读
}

/// Hook 动作
pub enum HookAction {
    Continue,            // 继续执行
    Modify,              // 已修改上下文
    Abort(String),       // 中止，返回消息
}

/// 可变上下文 (BeforeRequest, AfterHistory, BeforeLLM)
pub struct MutableContext<'a> {
    pub session_key: &'a str,
    pub messages: &'a mut Vec<ChatMessage>,
    pub user_input: Option<&'a str>,
    pub vault_values: Vec<String>,
}

/// 只读上下文 (AfterToolCall, AfterResponse)
pub struct ReadonlyContext<'a> {
    pub session_key: &'a str,
    pub messages: &'a [ChatMessage],
    pub response: Option<&'a str>,
    pub tool_calls: Option<&'a [ToolCallInfo]>,
    pub token_usage: Option<&'a TokenUsage>,
    pub vault_values: Vec<String>,
}

/// Hook Trait
#[async_trait]
pub trait PipelineHook: Send + Sync {
    fn name(&self) -> &str;
    fn point(&self) -> HookPoint;
    async fn run(&self, ctx: &mut MutableContext<'_>) -> Result<HookAction, AgentError>;
    async fn run_parallel(&self, ctx: &ReadonlyContext<'_>) -> Result<HookAction, AgentError>;
}
```

### 15.3 内置 Hooks

```rust
/// Vault Hook - 自动注入密钥到消息
pub struct VaultHook {
    injector: Arc<VaultInjector>,
}

/// External Shell Hook - 执行外部脚本
/// 文件位置:
///   ~/.gasket/hooks/pre_request.sh   → BeforeRequest
///   ~/.gasket/hooks/post_response.sh → AfterResponse
pub struct ExternalShellHook {
    script_path: PathBuf,
    point: HookPoint,
}
```

### 15.4 执行策略

```rust
impl HookRegistry {
    /// 顺序执行 (BeforeRequest, AfterHistory, BeforeLLM)
    pub async fn execute(
        &self,
        point: HookPoint,
        ctx: &mut MutableContext<'_>,
    ) -> Result<HookAction, AgentError> {
        for hook in self.hooks_for(point) {
            match hook.run(ctx).await? {
                HookAction::Abort(msg) => return Ok(HookAction::Abort(msg)),
                HookAction::Continue => { /* modifications applied via mutable ctx */ }
            }
        }
        Ok(HookAction::Continue)
    }

    /// 并行执行 (AfterToolCall, AfterResponse)
    pub async fn execute_parallel(
        &self,
        point: HookPoint,
        ctx: &ReadonlyContext<'_>,
    ) -> Vec<Result<HookAction, AgentError>> {
        let hooks = self.hooks_for(point);
        let futures: Vec<_> = hooks.iter()
            .map(|h| h.run_parallel(ctx))
            .collect();
        futures::future::join_all(futures).await
    }
}
```

---

## 16. Heartbeat 模块

### 16.1 架构概述

Heartbeat 是 Gasket 的**主动唤醒系统**，通过解析 `HEARTBEAT.md` 文件中的待办任务实现定期自驱：

```
┌─────────────────────────────────────────────────────────────────┐
│                   Heartbeat System                               │
│                                                                  │
│  ┌──────────────┐     ┌──────────────────────────────────────┐ │
│  │ HEARTBEAT.md │────▶│       HeartbeatService               │ │
│  │              │     │                                      │ │
│  │ - [ ] 任务1  │     │  1. 读取文件，解析待办任务            │ │
│  │ - [x] 任务2  │     │  2. 对每个任务执行回调                │ │
│  │ - [ ] 任务3  │     │  3. 自动清理已完成项                  │ │
│  └──────────────┘     │  4. 按间隔重复                        │ │
│                       └──────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────────────┘
```

### 16.2 核心实现

```rust
/// 心跳服务
pub struct HeartbeatService {
    workspace: PathBuf,
    interval_secs: u64,                       // 默认 30 分钟
}

impl HeartbeatService {
    pub fn new(workspace: PathBuf) -> Self;
    pub fn with_interval(workspace: PathBuf, interval_secs: u64) -> Self;

    /// 读取待办任务
    pub async fn read_tasks(&self) -> Vec<String> {
        let path = self.workspace.join("HEARTBEAT.md");
        let content = tokio::fs::read_to_string(&path).await?;

        // 解析 - [ ] 待办项
        content.lines()
            .filter(|line| line.trim().starts_with("- [ ]"))
            .map(|line| line.trim().trim_start_matches("- [ ]").trim().to_string())
            .filter(|task| !task.is_empty())
            .collect()
    }

    /// 主循环
    pub async fn run<F, Fut>(&self, mut callback: F)
    where
        F: FnMut(String) -> Fut + Send,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let mut interval = tokio::time::interval(
            Duration::from_secs(self.interval_secs)
        );

        loop {
            interval.tick().await;

            let tasks = self.read_tasks().await;
            for task in tasks {
                callback(task).await;
            }

            // 清理已完成项
            self.compact_file().await;
        }
    }

    /// 自动清理已完成的 - [x] 项
    async fn compact_file(&self) {
        // 移除所有 - [x] 行，保留 - [ ] 行
    }
}
```

### 16.3 Gateway 集成

```rust
// 在 Gateway 启动时注册心跳服务
let heartbeat = HeartbeatService::with_interval(workspace.clone(), 1800); // 30分钟

tokio::spawn(async move {
    heartbeat.run(|task| {
        let inbound_tx = inbound_tx.clone();
        async move {
            // 将待办任务注入到消息总线
            let msg = InboundMessage::heartbeat(&task, &default_target);
            let _ = inbound_tx.send(msg).await;
        }
    }).await;
});
```

---

## 附录: 模块交互图

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                            请求处理完整流程                                  │
└─────────────────────────────────────────────────────────────────────────────┘

User Input
    │
    ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│ Bus Layer                                                                   │
│  ┌──────────────┐    ┌──────────────┐    ┌──────────────┐                  │
│  │Router Actor  │───▶│Session Actor │───▶│Outbound Actor│                  │
│  │              │    │              │    │              │                  │
│  │ - Route      │    │ - Serialize  │    │ - HTTP Send  │                  │
│  │ - Spawn      │    │ - Call Agent │    │ - Fire/Forget│                  │
│  └──────────────┘    └──────┬───────┘    └──────────────┘                  │
└─────────────────────────────┼───────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│ Session Layer                                                               │
│  ┌────────────────────────────────────────────────────────────────────┐    │
│  │ prepare_pipeline()                                                  │    │
│  │  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐  ┌───────────┐  │    │
│  │  │BeforeRequest│→│Save User Msg│→│Load History │→│Assemble   │  │    │
│  │  │   Hooks     │  │             │  │+ Process    │  │Prompt     │  │    │
│  │  └─────────────┘  └─────────────┘  └─────────────┘  └───────────┘  │    │
│  │                               │                                    │    │
│  │                               ▼                                    │    │
│  │  ┌────────────────────────────────────────────────────────────┐   │    │
│  │  │ kernel::execute_streaming()                                 │   │    │
│  │  └────────────────────────────────────────────────────────────┘   │    │
│  │                               │                                    │    │
│  │                               ▼                                    │    │
│  │  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐               │    │
│  │  │Save Response│→│ Try Compact │→│AfterResponse│               │    │
│  │  │             │  │             │  │   Hooks     │               │    │
│  │  └─────────────┘  └─────────────┘  └─────────────┘               │    │
│  └────────────────────────────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│ Storage Layer                                                               │
│  ┌──────────────┐    ┌──────────────┐    ┌──────────────┐                  │
│  │SQLite Event  │    │SQLite Memory │    │Markdown Files│                  │
│  │Store         │    │Index         │    │             │                  │
│  └──────────────┘    └──────────────┘    └──────────────┘                  │
└─────────────────────────────────────────────────────────────────────────────┘
```
