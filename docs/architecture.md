# conga 架构设计

> 对应 workspace 版本 `2.0.0` · 仓库 [YeHeng/conga](https://github.com/YeHeng/conga) · MIT
>
> 本文面向想理解 conga 内部结构、做二次开发或集成的工程师。若只想安装使用,请阅读 [使用文档](./usage.md)。

---

## 1. conga 是什么

conga 是一个**轻量级、可自托管的个人 AI 助手框架**(自述:*"A lightweight personal AI assistant framework"*)。它把"一个能调用工具、能流式输出、能管理会话与权限的 LLM agent"做成了分层可复用的 Rust 工作区,并配有一个 Vue 3 的 Web / 桌面前端。

关键词(workspace `Cargo.toml`):`ai / agent / chatbot / llm`。

### 设计哲学

conga 的内核借鉴 "pi-style" 的可插拔 agent 设计,核心有三条原则,贯穿整个代码库:

| 原则 | 含义 | 体现在 |
|---|---|---|
| **loop 是无状态纯函数** | agent 推理循环不持有状态,状态全部由上层 host 持有 | `agent_loop` 只接收 `AgentContext` + `AgentLoopConfig`,返回新消息 |
| **provider 通过依赖注入接入** | 内核不知道"具体哪家 LLM",只认一个 `StreamFn` trait | `AgentLoopConfig.stream_fn` |
| **插件用进程内 Rust crate,而非动态加载** | 额外工具 / hook 由扩展 crate 在启动时 `register`,可选挂 Cargo feature | `conga-ext` + `ExtensionApi` |

> 这三条决定了 conga 的可测试性(注入 mock provider)、可复用性(同一套 host 同时驱动 CLI 和 Web 网关)和可扩展性(加工具不必改内核)。

---

## 2. 顶层架构与 Crate 分层

conga 后端是一个 Cargo workspace(`conga/Cargo.toml`),包含 5 个 crate,呈"内核 → 宿主 → 前端壳"的严格分层。

### 依赖关系图

```
                         ┌──────────────────────────────────────────┐
                         │            conga (内核)             │
                         │  agent_loop · types · tools · providers   │
                         │  extension · storage                      │
                         └────────────────────┬─────────────────────┘
                                              │ 依赖
                ┌─────────────────────────────┴──────────────────────────┐
                ▼                                                          ▼
   ┌─────────────────────┐                                       ┌────────────────────┐
   │    conga-host      │                                       │    conga-ext      │
   │ config · session    │                                       │  hello · todo      │
   │ permission · compact│                                       │  search            │
   │ hooks · external    │
   └──────────┬──────────┘                                       └─────────┬──────────┘
              │                                                            │ (可选 feature)
   ┌──────────┴──────────────────────┐                                     │
   ▼                                 ▼                                     ▼
┌──────────────────┐         ┌──────────────────┐               ┌──────────────────┐
│  conga-gateway  │         │   conga-cli     │◄──────────────┘  ext feature
│ (bin, WS 网关)   │         │  (bin, REPL)     │
│ core + host      │         │ host + core      │
└──────────────────┘         └──────────────────┘
        │
        ▼  (WebSocket + REST)
┌──────────────────────────────────────────────────────────────┐
│                web/  (Vue 3 + Vite + Tauri 2)                 │
│   浏览器应用  ─── WS/HTTP ──┐                                │
│   Tauri 桌面壳 ─── IPC ─────┴─ 同一份 src,运行时 isTauri 分支│
└──────────────────────────────────────────────────────────────┘
```

### 各 crate 职责一览

| crate | 类型 | 职责 | 关键依赖 |
|---|---|---|---|
| **`conga`** | lib(`conga`) | 内核:agent loop、消息/事件/工具类型、内置工具、LLM provider、扩展 API、JSONL 存储。**无内部依赖** | reqwest、ignore、glob、regex、async-stream |
| **`conga-host`** | lib | 可复用宿主层:配置加载、会话管理、权限策略、hook 组合、上下文压缩、外部工具桥接、事件渲染。把 loop 装进一个 `Host` 驱动器 | `conga` |
| **`conga-ext`** | lib | 可选的进程内扩展 crate(`hello`/`todo`/`search`/`permission_gate`),启动时经 `ExtensionApi` 注册工具与 hook | `conga` |
| **`conga-gateway`** | bin(`conga-gateway`) | WebSocket 网关服务器,把 Vue 前端桥接到 agent loop,并提供 REST 上下文接口 | `conga` + `conga-host`、axum |
| **`conga-cli`** | bin(`conga`) | 交互式终端 REPL agent,每行输入调一次 `run_turn`。带斜杠命令 | `conga-host` + `conga` + 可选 `conga-ext`、reedline |

> **两个二进制要分清**:包名是 `conga-cli`,但产出的二进制名是 **`conga`**;另一个二进制是 **`conga-gateway`**。

### 分层原则

- **`core` 是接缝最少的纯内核**:它不知道"谁来调用""配置从哪来""结果如何渲染"。这些全由 `host` 决定。
- **`host` 是可复用胶水**:同一套 `Host::run_turn` 既驱动 CLI 的终端打印,也驱动 gateway 的 WebSocket 推流——区别只在 `on_event` 回调的实现。
- **`gateway` / `cli` 是两种"前端壳"**:各自负责传输(WS / stdin)和呈现,共用 `host`。

---

## 3. 核心概念

| 概念 | 定义 | 代码位置 |
|---|---|---|
| **Session** | 一次连续对话,对应磁盘上 `~/.conga/sessions/<id>/events.jsonl` 的一份 append-only 事件日志(唯一真相源) | `host/src/session.rs`(`SessionManager`) |
| **SessionEvent** | 事件日志的追加写词汇表:`TurnStart` / `User` / `Assistant{message,usage}` / `ToolResult` / `TurnEnd{reason}` / `Cleared` / `Compacted{base,dropped}` | `conga/src/types/session_event.rs:15` |
| **derive_messages** | 纯投影:事件日志 → 模型可见消息列表(`TurnStart`/`TurnEnd` 不产出消息;投影从最后一个 `Cleared`/`Compacted` 之后重启) | `conga/src/types/session_event.rs:94` |
| **EventStorage** | `events.jsonl` 的追加写 / 读取存储:`O_APPEND` 单次 `write_all`、torn-tail 自愈、未知变体 fail-closed、原子批量安装(tmp+rename) | `core/src/storage/mod.rs:323` |
| **Host** | 把 config/session/policy/hooks/stream_fn 组装在一起的驱动器;对外暴露 `run_turn`(历史从日志派生,不由调用方携带) | `host/src/lib.rs:51` |
| **Agent Loop** | 无状态的推理循环:调 LLM → 解析响应 → 执行工具 → 把工具结果喂回 → 直到结束或超限;经注入的 `persist` 回调逐事件落盘 | `core/src/agent_loop.rs` |
| **Tool** | 一个带 JSON Schema 参数、风险等级、执行闭包的函数,LLM 可主动调用 | `core/src/types/tool.rs`(`ToolDefinition`) |
| **Hook** | 围绕每次工具调用的拦截器:`before_tool_call` 可 Allow/Block/Modify,`after_tool_call` 可改写结果(如脱敏) | `core/src/types/tool.rs`(`HookChain`) |
| **Provider** | 一个实现了 `StreamFn` 的 LLM 客户端;内核只认这个 trait,不认具体厂商 | `core/src/providers/mod.rs` |
| **Compaction** | 在喂给 LLM 之前压缩工作 transcript;压缩结果作为 `Compacted` 检查点**追加**到事件日志(日志仍 append-only,但投影从最后一个检查点重启);预算从日志尾部恢复 | `conga-host/src/compact.rs`(`ContextBudget`) |
| **Gateway** | 每条 WebSocket 连接 = 一个会话;内联 `Host::run_turn` 驱动 agent loop,经 select! 多路复用推事件回 WS;历史按需从日志 `derive_messages` | `conga-gateway/src/ws.rs` |

---

## 4. 请求生命周期(数据流)

conga 有两条入口路径,但都汇聚到同一个 `Host::run_turn` → `run_agent_loop`。

### 4.1 共同内核:`run_turn`

`Host::run_turn(user_msg, on_event)`(`host/src/lib.rs:187`)是整条流水线的枢纽。**历史不从调用方传入**——它由日志派生,磁盘 `events.jsonl` 是唯一真相源:

```
run_turn(user_msg, on_event)
   │
   1. session.migrate(sid)              ← 写路径:读 events.jsonl(必要时迁移旧 messages.jsonl)
      session.append_event(TurnStart)     ← 框定本轮开始
      session.append_event(User)          ← 用户消息先于循环落盘
   │
   2. history = derive_messages(events)   ← 纯投影:事件日志 → 模型可见消息
      budget 从日志尾部恢复(最后一条 Assistant 的 usage.input_tokens)
      history = budget.compact(&history)  ← 超预算则追加一条 Compacted 检查点
                                           (base = 压缩后视图,dropped = 丢弃条数);
                                           日志仍 append-only,投影从最后一个检查点重启
   │
   3. cfg.prepare_turn(TurnInputs{ system_prompt, history, tools, cwd, session_id },
                        signal, hooks, stream_fn, max_turns, Some(session.persist_fn()))
        └─ persist 回调注入 AgentLoopConfig.persist(context.rs:68);逐事件同步落盘
   │
   4. run_agent_loop(vec![user], context, config, on_event)
        └─ 无状态推理循环(见 4.3)。persist 按崩溃安全序调用:
           Assistant(含 usage) 先于其中任何工具执行落盘;每个 ToolResult 定稿后即落盘
   │
   5. session.append_event(TurnEnd{reason})   ← 成功 / 失败 / 中止皆落盘(永远不静默半截对话)
   │
   └─ 返回 TurnSummary{ reason, new_messages }(reason: Completed / Aborted{cause} / Error{message})
```

关键不变量:**`history` 每轮从日志现派生**(`derive_messages`),调用方不再持有一份内存 transcript;**磁盘是逐事件追加**——已发生的副作用(助手消息、工具结果)在它们发生时就落盘,崩溃 / 失败 / 取消的轮次仍保有其已经发生的全部事实。`persist: None` 路径(裸 `agent_loop` / 既有测试)不落盘,行为与之前逐字节一致。

**检查点(checkpoint)**:日志里有两种事件会让投影重启,取**最后一个**生效——`Cleared`(`/clear` 重置为空白对话)与 `Compacted`(压缩检查点,携带压缩后的视图 `base`)。检查点**之前**的事件仍完整躺在日志里,只是不再投影给模型;`derive_messages` / `view_scope_start` 都以「最后一个检查点之后」为作用域边界。

**两种作用域,两个名字**:`view_scope_start`(视图范围,`Cleared`|`Compacted` 之后)用于「模型还看得见什么」;`billing_scope_start`(计费范围,仅 `Cleared` 之后)用于「这通对话一共花了多少 token」——压缩只缩短模型的窗口,不会让已计费的事实消失。两者刻意不同,不要合并。

### 4.2 路径 A:CLI REPL

```
用户在终端敲一行
   │  Reedline 读行 (cli/src/main.rs:103)
   ▼
若是 / 开头 → 斜杠命令 (/mode /resume /clear /sessions /reload-tools)
否则:
   │  工作历史与压缩都在 run_turn 内部从日志现派生
      (cli/src/main.rs:116 起的 host.run_turn 调用)
   ▼
host.run_turn(user_msg, |ev| {
     printer.on_event(&ev);                       ← 终端实时渲染
})
   │  ← persist 回调逐事件落盘(events.jsonl);usage 随 Assistant 事件持久化
   ▼
(日志即真相源:CLI 不再维护内存 transcript)
```

### 4.3 内核循环:`run_agent_loop` 单轮结构

```
for turn in 0..max_turns {                       ← 外层循环,受 CONGA_MAX_TURNS 限制
    若 signal 被置位 → 协作式中止,返回已累积的 partial transcript

    steer.drain():                              ← 中途转向(AgentLoopConfig.steer)
        每条排队的用户文本 → persist(User) + 追加为真实 User 消息
        (重启可还原:走事件日志,不是旁路内存)

    stream = stream_fn.stream(model, messages, system_prompt, tools, signal)
              └─ 仅在"流出首个 chunk 之前"的失败才重试 (RetryPolicy;
                 429 按更长退避下限 + jitter 处理);
                 流中途失败则直接上报,不重试(避免重复输出)
    consume(stream):                              ← 消费 StreamChunk 流
        TextDelta      → on_event(MessageUpdate) + 累积 assistant 文本
        ToolCallDelta  → 累积工具调用 (id/name/args)
        ThinkingDelta  → on_event(思考过程)
        Usage{in,out}  → 记录用量
        Done / Error   → 结束本turn
    persist(SessionEvent::Assistant{message, usage})   ← 崩溃安全:先于本消息内任何工具执行(agent_loop.rs:64)

    若 stop_reason == ToolUse 且未超 max_tool_calls_per_turn:
        for each tool_call:
            verdict = hooks.before_tool_call(id, name, args, risk)   ← 异步,可能等人审批
                Allow | Block(reason) | Modify(new_args)
            match verdict:
                Allow/Modify → execute(tool) → after_tool_call(result) → 追加 ToolResult
                Block        → 追加带 reason 的 ToolResult(不执行)
        (每个 ToolResult 经 record_tool_result 定稿后即 persist;落盘序 == 执行序)
        把所有 ToolResult 加入 messages,继续外层循环(再问 LLM)

    若 stop_reason == EndTurn → 跳出循环
}
on_event(AgentEnd);  返回本轮新增消息
```

### 4.4 路径 B:Gateway(WebSocket)

```
浏览器/桌面端 ──WS──► /ws?user_id=<chatId>
   │  每条 WS 连接 = 一个 session (conga-gateway/src/ws.rs)
   ▼
收到 {"type":"message","content":"...","trace_id":"..."}
   │  内联 host.run_turn(select! 多路复用事件转发 + cancel/approval)
   ▼
forwarder 任务: AgentEvent → event_to_ws() → JSON → 推回 WS
   │     (thinking / tool_start / tool_end / content / error / done)
主任务 select! 入站: {"type":"cancel"} → 置 signal 中止
                    {"type":"approval_response",...} → 唤醒挂起的审批等待
```

需要人工审批时,gateway 会向客户端推 `approval_request`,然后用 `ApprovalRegistry`(oneshot + cancel + 超时三路等待)挂起,直到客户端回 `approval_response`、用户取消、或超时。

---

## 5. conga 内核详解

内核导出见 `core/src/lib.rs`。

### 准入标准:什么才进 core

一个能力想进入 `conga`,先过三问;任何一问答不上来,它就属于 `host`、`ext` 或某个 feature 之后:

| # | 问题 | 判据 |
|---|---|---|
| 1 | **三个宿主都要吗?** | CLI、gateway、桌面端都用到的能力才是内核;只有一两个宿主需要的是宿主层服务(如 skills 目录注入、session 检索引擎 → `host`)或 agent 工具(→ `ext`) |
| 2 | **依赖代价是什么?** | 零新增依赖直接进;重依赖必须 opt-in feature 且默认关闭(如 Linux Landlock → `sandbox-landlock`),默认构建零影响 |
| 3 | **持有的是什么状态?** | 配置状态(可注入、无资源持有,如 `proxy.rs` 的 override、`guard.rs` 的重复计数)可以进 core;资源状态(进程句柄、连接池、会话注册表、可重建的派生数据)属于宿主或扩展层 |

先例:PTY 会话注册表是进程状态 → 整个 `terminal` 工具住 `conga-ext` feature 之后;FTS5 索引是可重建派生数据 → 引擎住 `conga-host` feature 之后;Landlock 是重依赖 → core 内 feature 之后,且无 feature 时 fail-closed。

### 5.1 类型系统(`types/`)

| 类型 | 作用 | 文件 |
|---|---|---|
| `AgentMessage` | 枚举 `User` / `Assistant` / `ToolResult`,一条对话的最小单元 | `types/message.rs` |
| `ContentBlock` | 消息内容块:`Text` / `ToolCall` / 图片等 | `types/message.rs` |
| `AgentEvent` | 内核向外发出的事件流:`MessageUpdate`、`ToolExecutionStart/End`、`AfterProviderResponse`、`TurnStart`… | `types/event.rs` |
| `ContentDelta` | 增量:`TextDelta` / `ToolCallDelta` / …(事件载荷) | `types/event.rs` |
| `SessionEvent` | **事件日志词汇表**(每行一条):`TurnStart` / `User` / `Assistant{message,usage}` / `ToolResult` / `TurnEnd{reason}` | `types/session_event.rs:15` |
| `TurnEndReason` / `CancelCause` | 轮次结束原因:`Completed` / `Aborted{cause}` / `Error{message}`;`CancelCause`:`User`/`Parent`/`Hook` | `types/session_event.rs:35,44` |
| `derive_messages` | 纯投影:事件日志 → 模型可见消息(`TurnStart`/`TurnEnd` 不产出消息) | `types/session_event.rs:69` |
| `ToolDefinition` | 工具定义:`name`/`label`/`description`/`parameters`(JSON Schema)/`risk`/`execute` | `types/tool.rs:28` |
| `RiskLevel` | `Low` / `Medium` / `High`(默认 `High`) | `types/tool.rs:18` |
| `HookChain` | 拦截器 trait:`before_tool_call`(async,返 verdict)+ `after_tool_call`(sync) | `types/tool.rs:154` |
| `ToolCallVerdict` | `Allow` / `Block(reason)` / `Modify(args)` | `types/tool.rs:130` |
| `AgentContext` | 一次 run 的输入:system_prompt、messages、tools、cwd、env、session_id | `types/context.rs:16` |
| `AgentLoopConfig` | 一次 run 的配置:model、thinking、max_turns、max_tool_calls_per_turn、signal、**stream_fn**、hooks、retry、**persist**(注入则逐事件落盘) | `types/context.rs:43` |
| `StreamFn` | **provider 接缝**:trait,`stream(model,messages,system,tools,signal) -> Stream<StreamChunk>` | `types/context.rs:232` |
| `StreamChunk` | provider 产出的事件:`TextDelta`/`ToolCallDelta`/`ThinkingDelta`/`Usage`/`Done`/`Error` | `types/context.rs:212` |
| `ModelSpec` / `ProviderApi` | 模型规格 + 协议族(`OpenAiCompat` / `Anthropic`) | `types/context.rs:184,193` |

> **为什么 `HookChain` 定义在 `types` 而不是 `extension`?** 这样 `AgentLoopConfig` 能持有 `Option<Arc<dyn HookChain>>` 而**不引入循环依赖**(concrete 实现是 `ExtensionApiImpl`)。

### 5.2 内置工具(`conga-host/src/tools/`)

> **归属纠偏**:内置工具住在 **conga-host**(`conga-host/src/tools/`),不在内核 crate。内核(`conga`)只持有工具的类型契约(`types/tool.rs`);审计文档 `core-module-audit.md` 描述的"core + `built-in-tools` feature"方案已被取代——工具是宿主层服务,不是三宿主共享的内核能力。

`built_in_tools()` 返回 9 个内置工具,均带风险分级:

| 工具 | 文件 | 用途 | 典型风险 |
|---|---|---|---|
| `read` | `tools/read.rs` | 读文件 | Low |
| `write` | `tools/write.rs` | 写文件 | High |
| `edit` | `tools/edit.rs` | 多 hunk 原子编辑(exact→fuzzy 定位,全有或全无) | High |
| `bash` | `tools/bash.rs` | **持久 shell**(每 session 一个 `sh`,`cd`/env 跨调用存活;`run_in_background` 后台执行,输出落 `state_dir/bg/*.log`;超时重置会话) | High |
| `grep` | `tools/grep.rs` | 正则搜索(基于 `ignore`,尊重 .gitignore) | Low |
| `list` | `tools/list.rs` | 列目录(基于 `ignore`+`glob`) | Low |
| `fetch` | `tools/fetch.rs` | HTTP GET URL,HTML 转可读 markdown 文本(30s 超时,200KB 截断) | Low |
| `todo` | `tools/todo.rs` | 多步任务工作记忆(add/list/toggle/clear,状态落 `state_dir`) | Low |
| `spawn_subagents` | `tools/subagent.rs` | 并行子 agent 编排(maxItems 5,见 §11) | Medium |

工具名冲突:装配层(`assembly.rs::dedup_tool_names`)按"首个注册胜出"(built-in → ext → external → MCP → append)去重并告警——循环按名字首匹解析,未上报的冲突会静默调错工具。
工具执行闭包签名(`ToolFn`):`Arc<dyn Fn(ToolCallCtx) -> Future<Output=Result<ToolResult,ToolError>>>`。`ToolContext.state_dir`(`~/.conga/sessions/<session>/tool_state/<tool>/`)是每个工具的**私有**状态目录;`ToolCallCtx.aborted()` 用于长循环里协作式中止。

### 5.3 LLM Provider(`providers/`)

- **`ProviderConfig`**(`providers/mod.rs:26`):从环境读取连接配置。必填 `CONGA_LLM_BASE_URL` / `CONGA_LLM_KEY` / `CONGA_LLM_MODEL`;`CONGA_LLM_API` 选 `openai`(默认)或 `anthropic`。
- **两个实现**,都实现 `StreamFn`:
  - `OpenAiCompat`(`openai_compat.rs`):OpenAI 兼容协议——DeepSeek、智谱、xAI、Groq、Ollama、vLLM 等。
  - `AnthropicProvider`(`anthropic.rs`):Anthropic 原生 messages API。
- **`sse.rs`**:SSE 流解析,把 HTTP chunk 流切成 `StreamChunk`。
- **代理**:支持 `CONGA_LLM_PROXY`(http+https 通吃)/ `CONGA_LLM_HTTP_PROXY` / `CONGA_LLM_HTTPS_PROXY`,按 scheme 取优先级。

### 5.4 扩展 API(`extension/`)

`ExtensionApi` 是扩展注册口。核心区分:**事件**是纯观察(emit 闭包,不返值),**hook** 返回 verdict 控制流程——两者在类型层不可混淆(`extension/api.rs`)。`ExtensionApiImpl` 同时是工具容器和 `HookChain` 实现。

### 5.5 存储(`storage/`)

两层存储并存:

- **`EventStorage`**(主,`storage/mod.rs:323`):会话存为 `events.jsonl`,每行一条 `SessionEvent`,由 `run_turn` 逐事件追加。写纪律:单次 `O_APPEND` 句柄 + 单次 `write_all` 的 `line\n`(`append_event` / `append_event_sync`);同步版本供 agent loop 的 `persist` 回调直接调用,无需桥接异步运行时。
- **`JsonlStorage`**(遗留,`storage/mod.rs:65`):旧 `messages.jsonl`,每行一条 `AgentMessage`。仅作为迁移源被读取(`load_messages`),迁移后删除;格式契约与 torn-tail 行为冻结不变。

**Torn-tail 自愈 + fail-closed**(`scan_jsonl`,`storage/mod.rs:217`):

- 最后一行解析失败(进程崩溃截断 = `Syntax`/`Eof`)→ 当作 torn tail 丢弃并截断文件,使后续追加落在干净数据之后(崩溃产物,非数据损坏)。
- 中间行损坏 → 报错带**文件 + 行号**(位腐 / 外部编辑 = 真实损坏)。
- `EventStorage.load_events` 额外开启 `fail_closed_on_data`:一条**完整**但 `type` 不匹配任何已知 `SessionEvent` 变体的行(`serde_json::error::Category::Data`)→ 加载失败带行号(版本错位,绝不当 torn tail 抹掉)。

**原子批量安装**(`append_events_atomic`,`storage/mod.rs:459`):整批先写 `events.jsonl.tmp` → `sync_all` → `rename`(POSIX 原子)。专用于**迁移**:旧 `messages.jsonl` 经 `SessionManager::migrate` 一次性包裹、原子写入 `events.jsonl`,成功后才 `delete_legacy` 删旧文件——崩溃要么只留 `.tmp`(下次重迁),要么 `events.jsonl` 已完整。详见 [ADR 0001](./adr/0001-event-sourced-session-log.md)。

**读路径不写盘**:`SessionManager::open` 是纯读的——有 `events.jsonl` 就读它,否则把旧 `messages.jsonl` **在内存里**转换后返回,不落盘、不删文件。只有写路径(`run_turn` → `migrate`)才安装迁移产物。因此一个 GET 请求永远不会改写用户的磁盘。

### 5.6 出站工具代理(`proxy.rs`)

| 模块 | 职责 | 关键导出 |
|---|---|---|
| `proxy.rs` | fetch / web_search 等工具出站 HTTP 流量的运行时可配代理 | `set_tool_proxy` / `tool_proxy` / `validate_tool_proxy` / `apply_tool_proxy` |

优先级:**进程内 override(桌面 UI 设置)> `CONGA_TOOL_PROXY` env > 无代理**。支持 scheme:http / https / socks5 / socks5h(可内嵌 `user:pass@`)。env 值非法时 **fail-open**(warn 后直连,不阻断工具);日志中凭据经 `redact` 脱敏(`http://***@proxy:8080`)。与 §5.3 的 LLM 代理(`CONGA_LLM_PROXY`)互不影响——一个管工具流量,一个管模型 API 流量。

---

## 6. conga-host 宿主层详解

宿主层把内核的"无状态循环"包装成一个有状态、可复用的驱动器,目录 `host/src/`。

### 6.1 `Host` 编排器(`lib.rs:51`)

`Host` 持有:配置 `HostConfig`、会话 `SessionManager`、权限策略 `Arc<PermissionPolicy>`、hook 链、协作中止信号 `Arc<AtomicBool>`、注入的 `stream_fn`、系统提示、工具列表、cwd、max_turns,以及压缩旋钮 `budget`(token 计数本身**不**留在 Host——每轮从日志尾部恢复,故 token 感知压缩跨重启存活)。

设计要点:

- **不持有 printer/writer**:渲染走 `run_turn` 的 `on_event` 回调,所以非终端前端(gateway)能驱动同一份代码。
- **`stream_fn` 默认取 provider 自身**;测试用 `with_stream_fn` 注入 fake。
- **`signal` 是共享中止旗**:每次 `Ctrl-C` 都被记录,`run_turn` 在下一轮重新清零。
- **hook 链可叠加**:`with_hooks` 让宿主在权限策略之上再压一层(如扩展的 pattern gate)。`Host::new` 默认 hook 链就是 `[policy]`。

### 6.2 各子模块

| 模块 | 职责 | 关键导出 |
|---|---|---|
| `config.rs` | 从 env 读取并组装 `HostConfig`(`ProviderConfig` + `AgentTunables` + system prompt + cwd),产出 `TurnInputs` | `ConfigLoader` / `HostConfig` / `TurnInputs` |
| `session.rs` | 会话 CRUD、列出、恢复(`resume`/`resume_last`)、事件追加、`open`(纯读:读 events.jsonl,旧 messages.jsonl 只在内存里转换)/`migrate`(写路径:首次打开迁移旧 messages.jsonl,失败 fail-closed)、clear(uuid 轮换);持有 `EventStorage` | `SessionManager` / `SessionInfo` |
| `permission.rs` | 权限策略:三档 `Mode` × 工具 `RiskLevel` 决策,内部持 approver 回调 | `Mode` / `PermissionPolicy` |
| `hooks.rs` | 把多个 `HookChain` 串成栈;`before` 取首个 Block / 末个 Modify,`after` 链式改写 | `HookStack` |
| `compact.rs` | 上下文压缩(见第 9 章) | `ContextBudget` / `compact_by_count` |
| `external_tool.rs` | 从 `CONGA_EXTERNAL_TOOLS` 白名单加载外部命令工具 | `ExternalToolBridge` / `commands_from_env` / `load_all` |
| `mcp.rs` | MCP(Model Context Protocol)客户端:连接外部 MCP 工具服务器(stdio),握手 → tools/list → tools/call | `McpBridge` / `load_all_mcp` / `McpServerConfig` |
| `printer.rs` | 把 `AgentEvent` 渲染到终端(含 Error 分支与 flush) | `EventPrinter` |
| `prompt.rs` | **编码 agent 系统提示**:`CODING_AGENT_PROMPT` 纪律文本、`append_project_doc`(向上找 AGENTS.md/CLAUDE.md,≤16KB)、`env_snapshot`(UTC 日期 + git status/diffstat,3s 超时防护) | `CODING_AGENT_PROMPT` / `append_project_doc` / `env_snapshot` |
| `preview.rs` | 审批 diff 预览:零依赖 LCS 行 diff,`edit` hunk 与 `write` 覆盖文件的 old→new 渲染 | `approval_preview` |
| `wire.rs` | 出站 wire 协议类型(`OutgoingEvent`):`thinking`/`tool_start`/`tool_end`/`content`/`error`/`done`/`busy`/`queued`/`approval_request(带 preview)` 的 JSON schema,网关与桌面端共用 | `OutgoingEvent` |
| `event_map.rs` | `AgentEvent` → `OutgoingEvent`(WS JSON)映射,含 10 种 `SubagentEvent` 转发 | `event_to_ws` / `subagent_event_to_ws` |
| `approval.rs` | 审批登记(`ApprovalRegistry`):在途审批 + "remember" 缓存,三路 select 等待决策 | `ApprovalRegistry` |
| `subagent.rs` | 子 agent 编排:`spawn_subagents` 工具的 host 侧 spawner(子日志持久化、全文结果提取) | `HostSubagentSpawner` |
| `settings.rs` | **web UI LLM 设置**:`~/.conga/settings.json` 读写(原子写、组校验、key 掩码、PUT 合并);`run_turn` 每轮经 `effective_provider` 重解析,fast 路由优先读它;`systemPrompt` 自定义基础指令(≤64KB,空=内置) | `EnvSettings` / `put_settings` / `load_settings` |

### 6.3 `install_ctrl_c`(`lib.rs:328`)

安装一个 SIGINT 处理器,把共享 `signal` 置位(协作式中止)。在 cooked tty 模式下流式输出中的 `Ctrl-C` 会被它捕获;在 prompt 行(raw 模式)下 `Ctrl-C` 是 reedline 的按键事件,不触发这里。

---

## 7. conga-gateway 网关详解

网关(`conga-gateway/src/`)是前端与内核之间的桥,基于 axum。自有模块仅 4 个:`main`(路由/启动)、`state`(共享 `AppState`)、`ws`(WS 连接处理)、`api`(REST);另有 `wire.rs`(仅入站协议类型)。出站 wire 协议(`OutgoingEvent`)、`AgentEvent`→WS JSON 映射(`event_map`)与审批登记(`approval`)都复用 **conga-host** 的模块(见 §6.2),桌面端走同一份实现。

### 7.1 启动与路由(`main.rs`)

- 初始化默认会话目录、加载 `.env`。
- 路由:

| 路由 | 方法 | 作用 |
|---|---|---|
| `/ws` | GET(升级 WS) | WebSocket 连接入口,每连接一会话 |
| `/api/sessions` | GET | 列出磁盘上所有会话(id / 消息数 / mtime),不依赖活跃 WS 连接 |
| `/api/commands` | GET | 斜杠命令列表(供前端补全) |
| `/api/settings` | GET / PUT | **web UI LLM 设置**(读写 `~/.conga/settings.json`):GET 返回掩码视图(key 只给 `apiKeySet`+`apiKeyHint`),PUT 走校验→合并(空 `apiKey`=保留旧值,`null` 组=清除)→原子写;下一次 LLM 调用即生效 |
| `/api/sessions/{key}/context` | GET | 上下文统计(token 占用、压缩标志、水印) |
| `/api/sessions/{key}/context/compact` | POST | 手动触发压缩(现已在 `run_turn` 内每轮从日志现算,此端点保留为前端兼容,返回最新统计) |
| `/api/sessions/{key}/name` | PUT | 重命名会话(原子写 `meta.json` 侧车) |
| `/api/sessions/{key}` | DELETE | 删除会话 |

- 端口 `CONGA_GATEWAY_PORT`(默认 **3000**),监听 `0.0.0.0`;CORS 放开。

### 7.2 连接模型(每连接一会话)

每条 WS 连接就是一个独立会话。收到 `"message"` 时:内联 `host.run_turn` 驱动 agent loop,一个 forwarder 任务把 `AgentEvent` 转 JSON 推回 WS;主任务进入 **select! 多路复用**,同时处理入站消息(cancel / approval_response)。

### 7.3 Wire 协议(前端 ↔ 网关)

**Client → Server**

```json
{ "type": "message", "content": "...", "trace_id": "..." }
{ "type": "cancel" }
{ "type": "approval_response", "request_id": "...", "approved": true, "remember": false }
```

**Server → Client**(每轮流式)

| `type` | 载荷 | 含义 |
|---|---|---|
| `thinking` | `content` | 思考过程增量 |
| `tool_start` | `name`,`arguments`,`tool_call_id` | 工具开始执行 |
| `tool_end` | `name`,`output?`,`error?`,`tool_call_id` | 工具结束/出错(`tool_call_id` 与 `tool_start` 配对;协议中**没有** `tool_id` 字段,回归测试锁定其缺席) |
| `content` | `content` | 助手文本增量 |
| `error` | `content?`,`message?` | 错误横幅 |
| `busy` | `content?`,`message?` | 一轮仍在进行时又收到新消息的回执(区别于 `error`,前端弹 toast 而不清状态) |
| `queued` | `message` | **中途转向回执**:turn 进行中收到的新消息已入列(steer),循环在下一次 LLM 调用前把它作为真实 User 消息注入并落盘;前端渲染为待处理用户气泡 |
| `done` | `usage_in?`,`usage_out?`,`elapsed_ms?` | 本轮结束;可带累计输入/输出 token 数与本轮耗时 |
| `approval_request` | `id`,`tool_name`,`description`,`arguments`,`preview?` | 请求人工审批;`preview` 为 `edit`/`write` 的行 diff 预览(零依赖 LCS),前端优先渲染 diff 而非原始 JSON |

### 7.4 审批(`conga-host/src/approval.rs`)

`ApprovalRegistry` 登记在途审批并维护 "remember" 缓存。`wait_for_decision` 用 **oneshot(用户决策)/ cancel(中止)/ 超时**三路 `select` 等待,避免闩锁毒化——`approval.rs` 内有专门的回归测试覆盖。

> **双通道取消**:协作中止用 `AtomicBool` 驱动 loop 退出,**同时**用 `watch` channel 解锁可能正挂起在审批上的等待,二者配合防止取消后闩锁泄漏。

---

## 8. 工具系统与权限模型

### 8.1 工具定义与风险

每个工具自带 `RiskLevel`(`Low`/`Medium`/`High`,默认 `High`,定义在 `ToolDefinition` 上)。这让 agent loop 能把风险转告 hook,**而不依赖一张硬编码的工具名表**(这正是 commit `336c8d3` "move tool risk to ToolDefinition, drop host risk_of table" 的意图)。

### 8.2 Hook 链与 Verdict

`HookChain` 在每次工具调用前后被咨询:

- **`before_tool_call`(异步)**:返回 `ToolCallVerdict`。组合规则:**首个 `Block` 获胜;否则末个 `Modify` 获胜;默认 `Allow`**。异步是为了支持"等人决策"(CLI 读 stdin / gateway WS 往返)。
- **`after_tool_call`(同步)**:纯变换,如脱敏/截断,可替换 `ToolResult`。

> **取消契约**:loop 挂在 `before_tool_call().await` 期间,abort 信号**不会**自动取消该 future;可能阻塞等人的实现必须自行检查信号或接受 cancel channel,置位时及时返回。

### 8.3 三档权限模式 × 三档风险

`Mode`(`host/src/permission.rs`):`Suggest` / `AutoEdit` / `FullAuto`,配合 approver 回调决定每个工具调用是自动放行、提示审批、还是直接阻断。默认值因入口而异:CLI 默认 `AutoEdit`(`--mode=` 可改),gateway 默认 `auto-edit`(`CONGA_GATEWAY_MODE`,见 ws.rs)。

典型决策矩阵(语义,具体以代码为准):

| | Risk=Low | Risk=Medium | Risk=High |
|---|---|---|---|
| **Suggest** | 提示审批 | 提示审批 | 提示审批 |
| **AutoEdit** | 自动放行 | 提示审批 | 提示审批 |
| **FullAuto** | 自动放行 | 自动放行 | 自动放行 |

审批入口:CLI 经 `stdin_approver`(`cli/src/main.rs:129`,stdin 读挪到 blocking 池避免卡 tokio worker);gateway 经 WS `approval_request`/`approval_response` 往返。

---

## 9. 上下文压缩(Compaction)

压缩是**纯宿主策略**(`conga-host/src/compact.rs`),目的是在喂给 LLM 前缩小工作 transcript。三个硬约束:

1. **日志只追加,压缩以「检查点」的形式追加**——`~/.conga/sessions/<id>/events.jsonl` 永远只追加、不改写已有行;压缩不修改历史,而是**追加一条 `Compacted` 检查点**(`base` = 压缩后的视图,`dropped` = 丢弃条数)。投影(`derive_messages`)从**最后一个** `Cleared` 或 `Compacted` 之后重启:被折叠的原文仍在磁盘上,只是不再投影给模型。
2. **无 LLM 摘要**——不调用模型做总结,只做"丢弃最旧的若干组 + 前置一条 `[compacted N earlier messages; original task kept]` 提示"。
3. **永不切断 tool_call ↔ result**——见 `atomic_groups`。
4. **pin 原始任务**(harness 约束)——首组(最初的用户任务)**永不丢弃**,钉在压缩输出最前;忘了为什么出发的 agent 比窗口短一点的 agent 更糟。
5. **老工具结果截断**(harness 约束)——最后一组之前的 `ToolResult` 文本块压到头部 400 字符 + 截断标记;最新交互保持逐字。截断只发生在压缩视图里;检查点之前的原始行仍完整留在日志中。

### 9.1 原子组(`atomic_groups`)

把消息切成 `[start, end)` 原子组:一条 `Assistant` 开一组,并把它**紧跟**的若干 `ToolResult` 吸收进同一组;其余消息各成单组。压缩以组为单位取舍,保证 `Assistant(tool_call)` 永不与其 `ToolResult` 分离(否则 LLM 会收到孤儿 tool_result 而报协议错误)。

### 9.2 两种触发模式

| 模式 | 触发 | 实现 |
|---|---|---|
| **Token 感知(主)** | provider 上报的 `usage.input_tokens` 超过 `window` 的 `threshold_pct`(默认 80%)时触发;压缩后留到 `target_pct`(默认 50%)——**带滞后**,避免在阈值附近反复压缩 | `ContextBudget`(`compact.rs:122`) |
| **条数兜底** | 当尚无 usage 数据(`last_input_tokens==0`)时,按消息条数 `CONGA_COMPACT_MAX_MESSAGES`(默认 80)压缩 | `compact_by_count`(`compact.rs:56`) |

`ContextBudget::compact` 在超阈值时,按 `target = messages.len() * target_pct / 100` 算出保留消息数,复用 `compact_by_count`(贪心保留最新整组 + 前置提示)。**一套算法,两个触发器**:token 感知(主)和条数兜底。无 tokenizer,不假装建模 per-message token 成本。

窗口取值优先级:**settings.json 的 `maxTokens` > `CONGA_CONTEXT_WINDOW` > 默认 128000**(`run_turn` 每轮重读设置并 `set_window`,保存即下一轮生效,无需重启)。

### 9.3 数据来源:从日志恢复预算

`usage.input_tokens` 随 `SessionEvent::Assistant { usage }` **持久化进事件日志**。`run_turn` 每轮从日志尾部恢复预算(最后一条 `Assistant` 事件的 `usage.input_tokens`,经 `ContextBudget::record_input_tokens` 喂入),再对 `derive_messages` 出的 `history` 跑 `compact`。因此 token 感知压缩**跨重启存活**——重启不再丢失 usage、退化成条数兜底。预算计数本身不留在 `Host`(见 §6.1)。这正是 commit `0ba96fc` 计划文档 "context-compaction" 的延伸:用 provider 真实 usage 替代估算,且让该 usage 持久化。

---

## 10. 前端架构(web/)

前端目录 `web/`,一套代码、两种形态:浏览器 Web 应用 + Tauri 桌面应用。

### 10.1 技术栈

| 维度 | 选型 |
|---|---|
| 框架 | Vue 3(Composition API + `<script setup lang="ts">`) |
| 构建 | Vite 7;生产 `vue-tsc -b && vite build`;别名 `@` → `./src` |
| 状态 | Pinia 3 |
| 样式 | Tailwind 3.4 + Less;shadcn-vue / radix-vue 模型,基于 HSL CSS 变量的 Token 体系 + 自定义 `th-*` 语义类 |
| UI 基件 | `radix-vue`(ScrollArea/Collapsible)、`@headlessui/vue`(菜单)、`lucide-vue-next`(图标)、`cn()` 类合并 |
| 内容渲染 | `marked` + `marked-highlight` + `highlight.js`(github-dark)+ `dompurify`(XSS 清洗)+ `mermaid`(图表,延迟加载) |
| 桌面运行时 | Tauri 2(`@tauri-apps/api` + `@tauri-apps/cli`) |
| 工具 | `@vueuse/core`;包管理标准化用 **pnpm** |

### 10.2 双形态:浏览器 + Tauri 桌面(同一份代码)

```
            web/src (同一份 Vue 代码)
               │
       ┌───────┴────────┐
       ▼                ▼
  Vite dev/build     Tauri 打包
  -> dist/            -> 桌面 App(.dmg/.msi/.exe)
       │                │
       ▼                ▼
  WS/HTTP -> gateway   Tauri IPC -> 进程内 Host
  (localhost:3000)     (src-tauri/chat.rs)
```

**关键事实**:Tauri 桌面端有**两种传输模式**,前端通过 `isTauri`(检查 `window.__TAURI_INTERNALS__`)在运行时自动切换:

- **浏览器模式**:经 `ws://<host>:3000` 的 WS/HTTP 与独立部署的 conga-gateway 通信。
- **桌面模式**:经 Tauri IPC(`invoke` + `chat-event` 监听)与 `src-tauri/src/chat.rs` 中的进程内 Host 通信,无需独立 gateway 进程。

桌面端共 11 个 `#[tauri::command]`:`chat.rs` 4 个(`send_message`、`cancel_turn`、`approval_response`、`get_context`),`lib.rs` 7 个(`list_sessions`、`get_session_messages`、`rename_session`、`delete_session`、`get_app_config`、`set_app_config`、`validate_proxy`)。前 8 个与 gateway 的 WS 消息类型和 REST 端点一一对应,确保前端逻辑共享;`validate_proxy` 在 UI 保存工具代理 URL 前按与 `conga::set_tool_proxy` 相同的规则校验(前端 `NetworkProxyDialog.vue` 弹窗,对应 §5.6 的运行时出站工具代理)。每个 session 拥有一个进程内 Host 实例(与 gateway 的 per-connection Host 完全一致:同一 config loader、system prompt、tool set、sub-agent wiring),事件经单一有序 IPC 通道(`WireEvent` 枚举 -> emitter task -> `app.emit`)流回前端。

**持久化完全由 Rust 后端拥有,桌面端不使用 localStorage**:会话记录由 Host 的 `persist_fn` 逐事件追加到 `~/.conga/sessions/{id}/events.jsonl`(append-only JSONL),显示名经 `rename_session` 原子写 `meta.json` 侧车,会话列表来自 `list_sessions`,删除即 `delete_session`——前端 chatStore 不再本地缓存任何会话记录。app 配置(主题、侧栏状态)由 `lib.rs` 的 `get_app_config`/`set_app_config` 读写 `~/.conga/app_config.json`(tmp+rename 原子写):前端 `storage.ts` 是内存 KV,桌面模式启动时(`initStorage`,在动态 import 应用模块图之前执行,避免 useTheme 模块级初始化竞争)从后端载入,写入防抖落盘。浏览器模式(无内嵌后端)仍以 localStorage 为持久层,同一套 `storage.ts` 接口写透。

> **部署含义**:浏览器模式需要独立 gateway;桌面模式自包含(进程内 Host 直接做推理),但桌面端仍需 LLM API key 和 `~/.conga` 配置。

### 10.3 项目结构(`web/src/`)

```
src/
├── main.ts            引导:createApp + Pinia + 样式
├── App.vue            根:可调宽/可折叠侧边栏 + 主聊天区
├── components/
│   ├── ui/            shadcn-vue 风格基件 (button/input/scroll-area/...)
│   ├── AppSidebar.vue            会话列表侧边栏
│   ├── ChatArea.vue        单聊天顶层容器
│   ├── ChatHeader.vue      状态/上下文条/主题/压缩 按钮
│   ├── ChatInput.vue       输入框 + 斜杠命令补全 + 发送/停止
│   ├── ChatTimeDivider.vue  消息时间分隔线
│   ├── MessageBubble.vue   消息渲染 (Markdown/mermaid/代码)
│   ├── MessageThoughtsPanel.vue  思考 + 工具调用时间轴
│   ├── ApprovalDialog.vue  工具审批模态框
│   ├── NetworkProxyDialog.vue  出站工具代理设置弹窗(见 §5.6)
│   └── SubagentThoughtsPanel.vue  子 agent 面板(已实现,见 §10.7)
├── composables/
│   ├── useChatSession.ts        核心:WS 处理/消息流/REST 上下文/发送/审批/停止
│   ├── useTheme.ts              模块级单例主题状态
│   └── useSidebar.ts            侧边栏拖拽/折叠,经 storage.ts 持久化
├── hooks/
│   ├── useIMWebSocket.ts      底层 WS 封装(连/重连/发/关)
│   └── useTauriChat.ts        Tauri IPC 通道(invoke + chat-event 监听)
├── stores/chatStore.ts          Pinia:全部聊天/消息/工具调用/子 agent 状态
├── lib/
│   ├── utils.ts                 cn() 类合并
│   ├── backend.ts               isTauri 分支:WS/REST 与 IPC 双通道抽象
│   ├── platform.ts              平台检测
│   ├── storage.ts               前端偏好 KV(桌面同步 app_config.json,浏览器 localStorage)
│   ├── markdown.ts              Markdown 渲染封装
│   └── notifications.ts         系统通知
├── styles/                      Less 主题 + Tailwind;themes/(亮/暗、5 色相、12 种 Markdown 风格)
└── types/index.ts               全部 TS 接口
```

### 10.4 与后端的双通道通信

前端用**两条**通道与 gateway 通信,均由 env 驱动:`VITE_WS_URL`(默认 `ws://localhost:3000`)、`VITE_API_URL`(默认 `http://localhost:3000`)。

**WebSocket(主,流式)** — `hooks/useIMWebSocket.ts`:

- 连接 URL:`${VITE_WS_URL}/ws?user_id=${chatId}` —— **`chatId` 被复用为网关的 `user_id` 会话标识**。
- 重连:指数退避,最多 5 次,之后显示手动 "Reconnect" 按钮。
- 入站消息(`composables/useChatSession.ts` 的 switch):`thinking` / `tool_start` / `tool_end` / `content` / `error` / `done` / `approval_request` / `subagent_*`(M2,惰性)。
- 出站消息:`{type:'message',content,trace_id}`、`{type:'cancel'}`、`{type:'approval_response',request_id,approved,remember}`。

**REST(辅,上下文元数据)** — `composables/useChatSession.ts`:

| 端点 | 作用 |
|---|---|
| `GET /api/sessions/{key}/context` | 拉取 `context_stats`(token 预算/占用百分比/压缩标志)与 `watermark_info` |
| `POST /api/sessions/{key}/compact` | 强制压缩 |
| `GET /api/commands` | 斜杠命令补全列表 |

其中 `{key}` = `encodeURIComponent("websocket:" + chatId)`,即 WS user-id 加 `websocket:` 前缀。

### 10.5 状态管理:三层、无全局中央 store

| 层 | 载体 | 职责 | 持久化 |
|---|---|---|---|
| 持久聊天域 | Pinia `chatStore` | 所有聊天/消息/工具调用/子 agent CRUD | 不本地持久化——会话由 Rust 后端拥有(桌面 `~/.conga/sessions/`,浏览器经 gateway 同一盘),前端经 REST/IPC 读写(见 §10.2) |
| 瞬时会话 | `useChatSession`(每聊天一个) | 连接状态机(`disconnected\|idle\|sending\|receiving`)、审批队列、子 agent 跟踪、5 分钟超时兜底 | 不持久化 |
| 主题 | `useTheme`(**模块级单例**,非 Pinia) | 亮/暗、5 色相、12 种 Markdown 风格 | `storage.ts` 偏好 KV:桌面同步 `~/.conga/app_config.json`,浏览器 `localStorage`(键 `conga_theme_v2` 等,见 §10.2) |

> 主题用自定义 `th-*` 工具类(`th-app-bg`/`th-text`/`th-border`/`th-gradient-brand`...)代替原始 Tailwind 配色,整张调色板可经 CSS 变量 + `data-hue`/`data-md-style` 属性整体切换。

### 10.6 渲染策略:流式刻意降级

流式输出期间(`isReceiving`),消息**只渲染为转义纯文本**(`MessageBubble.vue`),避免每个 chunk 都跑一遍 `marked.parse + DOMPurify`。完整的 Markdown / Mermaid 渲染**只在流结束后**触发。这是用一次首屏流畅度换渲染开销的务实取舍。

### 10.7 子 agent 面板(已实现)

前端内置完整的 `subagent_*` 消息类型、store 字段与 switch 分支(`types/index.ts`、`useChatSession.ts`)。当 `spawn_subagents` 工具被调用时,gateway 经 `event_map::subagent_event_to_ws` 将 10 种 `SubagentEvent` 转发为 WS JSON,前端 `SubagentThoughtsPanel` 实时渲染子 agent 的思考与工具调用时间轴(运行中与完成后同一面板展示详情)。子 agent 编排实现见 `host/src/subagent.rs`(`HostSubagentSpawner`)与 `core/src/subagent.rs`(`SubagentSpawner` trait)。

---

## 11. 扩展机制(conga-ext)与子 agent 编排

`conga-ext` 是可选的进程内扩展 crate,启动时经 `ExtensionApi` 注册工具与 hook:

- `register_all(&mut api)` 把 `hello` / `search` / `permission_gate` 注册进去(`todo` 已转正为内置工具,住 `conga-host/src/tools/todo.rs`)。
- CLI 通过 Cargo feature `ext`(`--features ext`)链接它;gateway 可类似接入。
- **事件 vs hook**:事件是纯观察(emit 闭包),hook 返回 verdict 控制流程,二者在类型层不可混淆(见 5.4)。
- 搜索扩展(`search.rs`)支持多家 provider:Brave / Tavily / **Serper(默认)** / SerpAPI / Exa / Firecrawl / DuckDuckGo,由 `CONGA_SEARCH_PROVIDER` + 对应 `*_API_KEY` 选择。
- **桌面 App 链接 conga-ext**:`web/src-tauri/src/chat.rs` 把 `conga_ext::search` 注册的 `web_search` 加入每个 session 的 tool set,其 HTTP client 遵守运行时工具代理(`conga::apply_tool_proxy`,见 §5.6)。

**子 agent 编排(harness 升级)**:

- **持久化**:每个子 agent 的运行落 `sessions/<parent>/sub/<uuid>/events.jsonl`(`HostSubagentSpawner::with_sub_log_root`),崩溃可恢复、事后可检视;父 agent 可用 `read` 工具读取其全文。
- **全文结果**:`SubagentResult.output` 携带全部 assistant 文本(非 200 字符摘要),`log_path` 指向子日志;父 agent 对完整产出推理。
- **fast 模型路由**:完整的 `CONGA_FAST_LLM_*` 环境集把子 agent 切到便宜模型(同一套 tunables);缺省集(打错字)在启动时 fail-loud 告警。
- 子 agent 工具集 = 内置工具减 `spawn_subagents`(禁嵌套),MCP/external 工具不给子 agent;共享权限策略仍逐调用把关。

> 想加自己的工具:实现一个返回 `Vec<ToolDefinition>`(+ 可选 `HookChain`)的注册函数,在宿主启动时调用;无需改内核。

---

## 12. 关键设计决策(Why)

| 决策 | 动机 |
|---|---|
| **`stream_fn` 依赖注入** | agent loop 与具体 LLM 彻底解耦,测试用 `MockStream` 注入 canned chunk 序列(`agent_loop.rs` 测试)即可,不必打真实网络 |
| **事件 vs hook 类型分离** | 观察与控流不可混淆:事件只 emit、hook 返 verdict。类型层强制,杜绝误用 |
| **事件溯源日志(逐事件追加)** | 副作用先于轮次完成落盘:崩溃 / 失败 / 取消的轮次仍保有已发生的全部事实(助手消息 + 工具结果)。`Assistant` 先于其中任何工具执行持久化(崩溃安全);`TurnEnd{reason}` 总是落盘。详见 [ADR 0001](./adr/0001-event-sourced-session-log.md) |
| **JSONL torn-tail 自愈 + fail-closed** | 末行解析失败 = 崩溃截断 → 丢弃并截断;中行损坏 = 真实损坏 → 报错带行号。事件日志额外 fail-closed:未知 `type` 变体(版本错位)→ 加载失败带行号,绝不当 torn tail 抹掉 |
| **审批双通道取消** | `AtomicBool` 驱动 loop 中止 + `watch` channel 解锁挂起审批,防止取消后闩锁毒化 |
| **前端 isTauri 双传输** | Tauri 桌面端通过 `isTauri` 运行时分支在 WS/HTTP(浏览器)与 IPC(桌面)之间切换,其中 8 个 `#[tauri::command]` 与 gateway 端点一一对应;单一 `WireEvent` 枚举 + emitter task 保证跨流有序,前端消息处理逻辑完全共享 |
| **压缩追加检查点、不改写日志;预算从日志恢复** | append-only 事件日志永远是真相源:压缩只**追加**一条 `Compacted` 检查点(`base` + `dropped`),已有行一字不改;投影从最后一个 `Cleared`\|`Compacted` 之后重启。工作内存压缩有损但不破坏 protocol(原子组保护 tool_call↔result)。`usage` 随 `Assistant` 事件持久化,token 预算每轮从日志尾部恢复,跨重启存活 |

---

## 13. 已知边界与预留

阅读本文时请注意以下**当前状态**,避免误判:

- **子 agent 已实现**:`spawn_subagents` 工具 + `HostSubagentSpawner`(host 层)+ gateway 事件转发 + 前端面板均已落地(见 §11)。CLI 暂未接入(无多路事件通道)。
- **MCP 支持 stdio + Streamable HTTP**:当前 `McpBridge`(stdio 子进程)与 `McpHttpClient`(HTTP POST)两种传输并存,可在 `mcp.json` 中混用。协议版本 `2025-06-18`(legacy era)。resources/prompts 原语、modern era(`2025-11-25`)为后续工作。
- **Linux 桌面构建未在 CI**:Tauri 桌面产物 CI 仅覆盖 macOS(`.dmg`)与 Windows(`.msi`/`.exe`);Linux 桌面需本机具备 webkit2gtk 等系统依赖自行构建。
