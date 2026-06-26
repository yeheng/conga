# 工具系统

> AI 的百宝箱——让 AI 能操作电脑、访问网络、管理文件

---

## 一句话理解

工具就是 AI 的**手和脚**，让 AI 不仅能"说话"，还能"做事"。

```mermaid
flowchart LR
    A[AI大脑] -->|需要查资料| B[网络搜索]
    A -->|需要读文件| C[文件操作]
    A -->|需要执行命令| D[命令执行]
    A -->|需要创建子任务| E[子代理]
    B --> A
    C --> A
    D --> A
    E --> A
```

---

## 为什么需要工具？

没有工具的 AI 就像一个**只有大脑、没有手脚**的人：

| 场景 | 没有工具 | 有工具 |
|------|---------|--------|
| 查今天天气 | "我无法获取实时信息" | [搜索网络] "今天北京25°C" |
| 分析你的代码 | "请把代码贴给我" | [读取文件] 直接分析项目 |
| 执行数据处理 | "请手动运行脚本" | [执行命令] 自动处理 |
| 获取最新新闻 | "我的知识截止到..." | [搜索网络] 获取实时新闻 |

```mermaid
flowchart TB
    subgraph 只有大脑
        A1[用户: 查天气]
        B1[AI: 对不起，我不知道]
    end
    
    subgraph 大脑+手脚
        A2[用户: 查天气]
        B2[AI: 稍等，我查一下]
        C2[调用天气工具]
        D2[获取数据]
        E2[AI: 今天25°C]
    end
    
    A1 --> B1
    A2 --> B2 --> C2 --> D2 --> E2
    
    style B1 fill:#FFCDD2
    style E2 fill:#C8E6C9
```

---

## 工具分类

```mermaid
mindmap
  root((AI工具箱))
    文件操作
      读取文件
      写入文件
      编辑文件
      列出目录
    网络访问
      网页搜索
      网页获取
    系统命令
      执行Shell
    历史查询
      关键词搜索
      语义搜索
    任务管理
      创建子代理
      创建计划
      记忆提取
      定时任务
    会话管理
      新建会话
      清空历史
    交互
      向用户提问
      发送消息
```

---

## 各类工具详解

### 1. 文件操作工具

让 AI 能读写你电脑上的文件：

```mermaid
sequenceDiagram
    participant U as 用户
    participant AI as AI
    participant F as 文件工具
    participant Disk as 电脑硬盘
    
    U->>AI: 帮我看看main.py
    AI->>F: 读取main.py
    F->>Disk: 读取文件内容
    Disk-->>F: 返回代码
    F-->>AI: 代码内容
    AI-->>U: 这段代码的功能是...
    
    U->>AI: 修复第10行的bug
    AI->>F: 编辑文件，修改第10行
    F->>Disk: 写入修改
    F-->>AI: 修改成功
    AI-->>U: 已修复，把+改成-了
```

**包含的工具：**

- `read_file` - 读取文件内容
- `write_file` - 创建/覆盖文件
- `edit_file` - 修改文件部分内容
- `list_dir` - 查看文件夹内容

**使用场景：**

- 代码审查和修改
- 配置文件编辑
- 批量文件处理
- 项目结构分析

### 2. 网络工具

让 AI 能上网查资料：

```mermaid
sequenceDiagram
    participant U as 用户
    participant AI as AI
    participant W as 网络工具
    participant Net as 互联网
    
    U->>AI: 今天有什么AI新闻？
    
    Note over AI: AI没有2024年后的知识
    
    AI->>W: 搜索"AI新闻"
    W->>Net: 调用搜索引擎API
    Net-->>W: 返回搜索结果
    W-->>AI: 10条相关新闻
    
    AI->>W: 获取第一条详情
    W->>Net: 访问网页
    Net-->>W: 返回网页内容
    W-->>AI: 详细内容
    
    AI-->>U: 今天的主要AI新闻有...
```

**包含的工具：**

- `web_search` - 搜索引擎（Brave/Tavily/Exa）
- `web_fetch` - 获取特定网页内容

**使用场景：**

- 获取实时信息
- 查阅最新文档
- 研究某个话题
- 验证事实

### 3. Shell 执行工具

让 AI 能执行系统命令：

```mermaid
sequenceDiagram
    participant U as 用户
    participant AI as AI
    participant S as Shell工具
    participant OS as 操作系统
    
    U->>AI: 运行测试
    AI->>S: 执行"npm test"
    S->>OS: 创建子进程
    OS-->>S: 返回测试结果
    S-->>AI: 测试通过/失败详情
    AI-->>U: 测试已完成，3个通过，1个失败...
```

**包含的工具：**

- `exec` - 执行 Shell 命令（带安全限制）

**使用场景：**

- 运行测试
- 构建项目
- 执行数据处理脚本
- 系统管理任务

**安全保护：**

```mermaid
flowchart TB
    Command[用户命令] --> Check{安全检查}
    Check -->|危险命令| Block[拒绝执行]
    Check -->|允许命令| Run[执行]
    Check -->|需要确认| Confirm[询问用户]
    Confirm -->|同意| Run
    Confirm -->|拒绝| Block
    
    Block --> Error[返回错误]
    Run --> Result[返回结果]
    
    style Block fill:#FFCDD2
    style Run fill:#C8E6C9
```

### 4. 历史查询工具

让 AI 能查询当前会话的对话历史：

```mermaid
sequenceDiagram
    participant U as 用户
    participant AI as AI
    participant H as HistoryQueryTool
    participant Store as SQLite 存储

    U->>AI: 我昨天说了什么？

    AI->>H: history_query(query="昨天")
    H->>Store: 按关键词搜索历史消息
    Store-->>H: 匹配的消息列表
    H-->>AI: 昨天用户说...

    AI-->>U: 你昨天提到了...
```

**包含的工具：**

- `history_query` - 按关键词查询当前会话的对话历史（SQLite 本地搜索）
- `history_search` - 语义搜索历史对话（需要 `embedding` 特性）

### 5. 子代理工具

让 AI 能创建"分身"处理复杂任务，支持**实时流式事件**和**模型选择**：

```mermaid
sequenceDiagram
    participant U as 用户
    participant Main as 主AI
    participant Spawn as 子代理工具
    participant Sub as 子AI
    participant Res as 结果汇总
    
    U->>Main: 分析这个项目的所有文件
    
    Note over Main: 文件太多，需要并行处理
    
    Main->>Spawn: spawn_parallel(tasks=[...])
    
    par 并行执行
        Sub->>Sub: 分析前端（实时事件→用户）
    and
        Sub->>Sub: 分析后端（实时事件→用户）
    and
        Sub->>Sub: 分析测试（实时事件→用户）
    end
    
    Sub-->>Res: 返回分析报告
    Res-->>Main: 汇总结果
    Main-->>U: 完整项目分析...
```

**包含的工具：**

- `spawn` - 创建单个子代理
  - 参数：`task`（任务描述，必填），`model_id`（可选，使用模型配置中的 profile ID）
- `spawn_parallel` - 并行创建最多 10 个子代理
  - 参数：`tasks`（任务列表，必填），支持简单字符串数组或带 `model_id` 的对象数组
  - 并发限制：由 `tools.spawn.max_concurrency` 控制（默认 1）

**实时流式事件（WebSocket 模式）：**

在 WebSocket 模式下，子代理的执行过程会实时推送到前端：

| 事件 | 说明 |
|------|------|
| `subagent_started` | 子代理启动，附带任务描述 |
| `subagent_thinking` | 子代理的思考过程 |
| `subagent_tool_start` | 子代理开始调用工具 |
| `subagent_tool_end` | 子代理工具调用完成 |
| `subagent_content` | 子代理生成的内容片段 |
| `subagent_completed` | 子代理完成，返回结果摘要 |

可通过 `agents.defaults.ws_summary_limit` 控制摘要长度（0 = 完整返回）。

### 6. 会话管理工具

**包含的工具：**

- `new_session` - 开启新会话，清空当前会话的所有历史消息和摘要，生成新的 session key（需要审批）
- `clear_session` - 清空当前会话历史（保留 session key）（需要审批）

### 7. 交互工具

- `ask_user` (`AskUserTool`) - 向用户提问并等待回复。参数：`question`（必填），`options`（可选，选项列表）。
- `message` (`MessageTool`) - 向用户发送消息（用于 Cron 等后台任务主动推送）

### 8. 系统工具

- `create_plan` (`CreatePlanTool`) - 创建执行计划。需要 LLM provider。参数：`goal`（必填）。

### 工具执行签名

所有工具通过统一的 `Tool` trait 执行，`ctx` 参数是**必需**的：

```rust
async fn execute(&self, args: Value, ctx: &ToolContext) -> ToolResult;
```

### 8. 定时任务工具

让 AI 能管理定时任务：

```mermaid
sequenceDiagram
    participant U as 用户
    participant AI as AI
    participant C as Cron工具
    participant Cron as Cron服务
    
    U->>AI: 每天早上9点提醒我喝水
    
    AI->>C: 创建定时任务
    C->>Cron: 添加任务
    Cron-->>C: 添加成功
    C-->>AI: 任务ID: reminder-001
    
    Note over Cron: 每天早上9:00
    
    Cron->>AI: 触发任务
    AI-->>U: [消息] 早上好！记得喝水哦
```

**包含的工具：**

- `cron` - 管理定时任务（增删改查）
- `script` (`PluginTool`) - 外部脚本工具（通过 YAML manifest 声明）

---

## 工具注册表

所有工具都登记在一个"工具箱"里：

```mermaid
flowchart TB
    subgraph 工具注册表
        R[工具箱]
        R --> T1[文件工具]
        R --> T2[网络工具]
        R --> T3[Shell工具]
        R --> T4[记忆工具]
        R --> T5[子代理工具]
        R --> T6[定时任务]
        R --> T7[历史查询]
        R --> T8[外部脚本]
    end

    AI[AI大脑] --> R

    AI -->|需要查资料| T2
    AI -->|需要改文件| T1
    AI -->|需要执行命令| T3
    AI -->|需要查历史| T7
    AI -->|需要扩展功能| T8
```

### 语义路由

AI 会自动选择最合适的工具：

```mermaid
flowchart TB
    Q[用户问题] --> Understand[理解意图]
    
    Understand -->|查资料| Search[网络搜索]
    Understand -->|改代码| File[文件编辑]
    Understand -->|运行测试| Shell[命令执行]
    Understand -->|查历史| History[历史查询]
    Understand -->|任务太复杂| Subagent[创建子代理]
    Understand -->|重新开始| Session[新会话]
    
    style Understand fill:#FFD700
```

---

## 工具执行流程

```mermaid
sequenceDiagram
    participant AI as AI大脑
    participant K as Kernel
    participant R as 工具注册表
    participant T as 具体工具
    participant E as 外部环境
    
    AI->>K: 我需要搜索"天气"
    
    K->>R: 查找"web_search"工具
    R-->>K: 返回工具实例
    
    K->>T: 执行搜索<br/>参数: "天气"
    
    T->>E: 调用搜索引擎API
    E-->>T: 返回结果
    
    T-->>K: 整理结果
    K-->>AI: 搜索完成，结果是...
```

---

## 工具使用示例

### 示例1：复杂编程任务

```mermaid
flowchart TB
    Start([开始]) --> A[用户: 帮我优化这个Python项目]
    
    A --> B[AI: 读取项目结构]
    B --> C[list_dir]
    C --> D[发现main.py, utils.py, tests/]
    
    D --> E[AI: 读取main.py]
    E --> F[read_file]
    F --> G[分析代码]
    
    G --> H[AI: 搜索Python最佳实践]
    H --> I[web_search]
    I --> J[获取优化建议]
    
    J --> K[AI: 执行性能测试]
    K --> L[exec: python benchmark.py]
    L --> M[获取性能数据]
    
    M --> N[AI: 应用优化]
    N --> O[edit_file]
    O --> P[保存优化后的代码]
    
    P --> Q[AI: 运行测试]
    Q --> R[exec: pytest]
    R --> S[测试通过]
    
    S --> T([完成: 项目已优化])
    
    style Start fill:#90EE90
    style T fill:#C8E6C9
```

### 示例2：研究性任务

```mermaid
sequenceDiagram
    participant U as 用户
    participant AI as AI
    participant S as 搜索
    participant F as 网页获取
    participant Mem as 记忆
    
    U->>AI: 研究一下最新的AI趋势
    
    AI->>S: 搜索"2024 AI趋势"
    S-->>AI: 10篇文章链接
    
    AI->>F: 获取前3篇文章
    F-->>AI: 详细内容
    
    AI->>AI: 总结要点
    
    Note over AI: 重要信息，值得保存
    
    AI->>Mem: 保存记忆<br/>"2024 AI趋势: ..."
    
    AI-->>U: 根据最新资料，2024年AI趋势有...
```

---

## 外部插件扩展

Gasket 还支持**外部插件**（通过 YAML 清单声明）：

```mermaid
flowchart TB
    subgraph Gasket
        AI[AI大脑]
        Plugin[插件系统]
    end
    
    subgraph 外部脚本
        S1[Python 脚本]
        S2[Node.js 脚本]
        S3[Shell 脚本]
    end
    
    AI --> Plugin
    Plugin --> S1
    Plugin --> S2
    Plugin --> S3
    
    S1 --> Plugin
    S2 --> Plugin
    S3 --> Plugin
    Plugin --> AI
```

**举例：**

- 用 Python 编写的天气查询工具
- 用 Node.js 编写的翻译工具
- 用 Shell 编写的系统监控工具

详见 [插件系统文档](plugin.md)。

---

## 工具审批系统

Gasket 内置了**工具执行审批**机制，防止 AI 在未经确认的情况下执行危险操作。

### 审批流程

```mermaid
sequenceDiagram
    participant AI as AI大脑
    participant R as 工具注册表
    participant CB as ApprovalCallback
    participant U as 用户/前端

    AI->>R: 执行 write_file
    R->>R: requires_approval = true
    R->>CB: request_approval
    CB->>U: 显示确认对话框
    U-->>CB: 同意 / 拒绝 / 记住决定
    CB-->>R: approved?
    alt 同意
        R->>R: 继续执行工具
    else 拒绝
        R-->>AI: PermissionDenied
    end
```

### 需要审批的工具

以下工具默认需要用户确认（WebSocket 模式显示确认对话框，CLI 模式直接执行）：

| 工具 | 类别 | 说明 |
|------|------|------|
| `write_file` | 文件系统 | 创建或覆盖文件 |
| `edit_file` | 文件系统 | 修改现有文件 |
| `exec` | 系统 | 执行 Shell 命令 |
| `new_session` | 会话 | 清空历史并新建会话 |
| `clear_session` | 会话 | 清空当前会话历史 |

**记住决策**：在 WebSocket 前端可以勾选"记住此决定"，同一会话中再次调用相同工具时将自动通过/拒绝。

### 免审批工具

以下只读工具无需确认，直接执行：

- `read_file`, `list_dir`, `web_search`, `web_fetch`
- `history_query`, `history_search`（需 embedding 特性）
- `spawn`, `spawn_parallel`
- `ask_user`, `message`
- `create_plan`

---

## 常见问题

**Q: AI 能随便执行任何命令吗？**
A: 不能。`exec` 等危险工具默认需要用户确认。可通过 `tools.exec.policy` 配置命令白名单/黑名单进一步限制。

**Q: AI 能访问我电脑上的所有文件吗？**
A: 默认可以访问任意路径，但可通过 `tools.restrict_to_workspace: true` 限制仅允许访问工作空间目录。

**Q: 工具执行失败怎么办？**
A: AI 会收到错误信息，然后决定重试、换种方式、或告诉用户出错了。

**Q: 怎么知道 AI 用了什么工具？**
A: 在流式输出中可以看到工具调用信息，比如"正在搜索..."、"正在读取文件..."。WebSocket 模式下还能实时看到子代理的执行进度。
