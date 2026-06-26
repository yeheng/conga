# 系统架构

> Gasket 整体架构设计——各部件如何协同工作

---

## 一句话理解

Gasket 就像一个**AI 助手的操作系统**，把用户输入、AI 大脑、记忆、工具连接在一起。

---

## 整体架构图

```mermaid
flowchart TB
    subgraph 用户层
        CLI[命令行终端]
        TG[Telegram]
        DC[Discord]
        SL[Slack]
        FS[飞书]
        WS[WebSocket]
    end

    subgraph 接入层
        Router[消息路由器]
    end

    subgraph 核心层
        Session[会话管理]
        Kernel[AI大脑]
        Hook[钩子系统]
    end

    subgraph 能力层
        Tools[工具箱]
        Memory[记忆库]
        Skills[技能库]
    end

    subgraph 外部服务
        LLM[LLM API<br/>GPT/Claude/DeepSeek]
        Web[互联网]
        File[本地文件]
    end

    CLI --> Router
    TG --> Router
    DC --> Router
    SL --> Router
    FS --> Router
    WS --> Router

    Router --> Session
    Session --> Kernel
    Session --> Hook

    Kernel --> Tools
    Kernel --> Memory
    Session --> Skills

    Kernel --> LLM
    Tools --> Web
    Tools --> File

    style Router fill:#E3F2FD
    style Session fill:#FFF3E0
    style Kernel fill:#F3E5F5
    style Memory fill:#E8F5E9
```

---

## 各层职责

### 1. 用户层：多种入口

```mermaid
flowchart LR
    subgraph 用户入口
        A[命令行]
        B[Telegram]
        C[Discord]
        D[其他渠道]
    end

    subgraph 统一处理
        E[统一消息格式]
    end

    A --> E
    B --> E
    C --> E
    D --> E

    style E fill:#E3F2FD
```

无论用户从哪个渠道进来，都会被转换成统一的消息格式：
- 用户ID
- 消息内容
- 渠道类型
- 时间戳

### 2. 接入层：消息路由

```mermaid
flowchart TB
    subgraph 消息路由
        R[Router]
    end

    subgraph Session 层
        S1[Session A]
        S2[Session B]
        S3[Session C]
    end

    subgraph Outbound 层
        Out[Outbound]
        TG[回复Telegram]
        DC[回复Discord]
    end

    R -->|用户A| S1
    R -->|用户B| S2
    R -->|用户C| S3

    S1 --> Out
    S2 --> Out
    S3 --> Out

    Out --> TG
    Out --> DC

    style R fill:#E3F2FD
    style Out fill:#FFF3E0
```

**关键设计**：
- 每个用户有独立的 Session
- Session 之间互不干扰
- 自动创建和清理 Session

### 3. 核心层：三剑客

```mermaid
flowchart TB
    subgraph 核心三剑客
        Session[会话管理<br/>大管家]
        Kernel[AI大脑<br/>思考者]
        Hook[钩子系统<br/>检查点]
    end

    User[用户] --> Session
    Session --> Hook
    Hook --> Kernel
    Kernel --> Session
    Session --> User

    style Session fill:#E3F2FD
    style Kernel fill:#FFF3E0
    style Hook fill:#F3E5F5
```

| 组件 | 比喻 | 职责 |
|------|------|------|
| Session | 管家 | 接待、准备资料、记录 |
| Kernel | 大脑 | 思考、决策、生成回复 |
| Hook | 检查点 | 安全检查、数据注入、日志 |

### 4. 能力层：工具箱

```mermaid
mindmap
  root((能力层))
    工具箱
      文件操作
      网络搜索
      命令执行
      子代理
    记忆库
      短期历史
      长期记忆
      用户画像
    技能库
      代码审查
      写作助手
      数据分析
```

---

## 数据流动

### 完整请求处理流程

```mermaid
sequenceDiagram
    participant U as 用户
    participant R as Router
    participant S as Session
    participant H as Hooks
    participant K as Kernel
    participant L as LLM
    participant T as Tools
    participant M as Memory

    U->>R: 发送消息
    R->>R: 路由到对应Session

    activate S
    S->>M: 加载用户记忆
    M-->>S: 返回记忆

    S->>H: BeforeRequest钩子
    H-->>S: 继续/中止

    S->>S: 保存用户消息到历史
    S->>S: 组装上下文

    S->>K: 请求处理

    activate K
    K->>L: 发送提示词
    L-->>K: 返回思考+可能的工具调用

    alt 需要工具
        K->>T: 调用工具
        T-->>K: 返回结果
        K->>L: 带上结果再请求
        L-->>K: 最终回复
    end

    K-->>S: 返回结果
    deactivate K

    S->>H: AfterResponse钩子
    S->>S: 保存AI回复
    S->>M: 更新访问记录

    S-->>R: 返回结果
    deactivate S

    R-->>U: 显示回复
```

---

## 模块详解

### Session：会话管理

```mermaid
flowchart TB
    subgraph Session内部
        A[接收请求]
        B[加载上下文]
        C[调用Kernel]
        D[保存结果]
    end

    subgraph 上下文组成
        S[系统提示]
        SK[技能]
        H[历史]
        M[记忆]
        Q[当前问题]
    end

    A --> B
    B --> S
    B --> SK
    B --> H
    B --> M
    B --> Q
    S --> C
    SK --> C
    H --> C
    M --> C
    Q --> C
    C --> D

    style Session内部 fill:#E3F2FD
```

### Kernel：AI 大脑

```mermaid
flowchart TB
    subgraph Kernel思考循环
        Start([开始]) --> Input[接收上下文]
        Input --> Ask[询问LLM]
        Ask --> Analyze{分析回复}

        Analyze -->|需要工具| Tool[执行工具]
        Tool --> Result[工具结果]
        Result --> Ask

        Analyze -->|直接回答| Output[输出结果]
        Analyze -->|达到上限| Output

        Output --> End([结束])
    end

    style Tool fill:#FFD700
```

### Tools：工具系统

```mermaid
flowchart TB
    subgraph 工具注册表
        R[ToolRegistry]
    end

    subgraph 各类工具
        F[文件工具]
        W[网络工具]
        E[执行工具]
        S[子代理]
        M[记忆工具]
    end

    R --> F
    R --> W
    R --> E
    R --> S
    R --> M

    F --> FS[本地文件]
    W --> Web[互联网]
    E --> Shell[Shell命令]
    S --> Sub[创建子AI]
    M --> Mem[读写记忆]
```

---

## 关键设计决策

### 1. 纯函数 Kernel

```mermaid
flowchart LR
    subgraph 输入
        A[上下文<br/>配置<br/>工具]
    end

    subgraph Kernel
        B[黑盒处理<br/>无副作用<br/>可预测]
    end

    subgraph 输出
        C[回复内容<br/>工具调用]
    end

    A --> B --> C

    style B fill:#C8E6C9
```

**好处**：
- 相同输入，相同输出
- 容易测试
- 方便重试和缓存

### 2. ContextBuilder 模式

```mermaid
flowchart TB
    subgraph 旧方法
        O["Option&lt;Context&gt;"]
        O -->|Some| P[持久化]
        O -->|None| S[无状态]
    end

    subgraph 新方法
        CB[ContextBuilder]
        CB -->|组装| CTX[完整上下文]
    end

    style CB fill:#C8E6C9
```

**好处**：
- 统一接口组装历史、记忆、摘要
- 支持三阶段 token 预算（bootstrap/scenario/on_demand）
- 代码更简洁清晰

### 3. 文件 + 数据库混合存储

```mermaid
flowchart TB
    subgraph Cron任务
        F[Markdown文件<br/>人类可读]
        D[SQLite状态<br/>机器高效]
    end

    subgraph 记忆
        F2[Markdown文件<br/>人类可编辑]
        D2[SQLite索引<br/>快速查询]
    end

    F <-->|配置| D
    F2 <-->|内容| D2
```

**好处**：
- 人类可编辑（Markdown）
- 机器高性能（SQLite）
- 版本控制友好

---

## 扩展点

### 1. Hooks：自定义行为

```mermaid
flowchart LR
    A[BeforeRequest] --> B[处理中]
    B --> C[AfterResponse]

    A --> A1[敏感词过滤]
    A --> A2[输入格式化]

    C --> C1[记录日志]
    C --> C2[发送通知]

    style BeforeRequest fill:#FFE0B2
    style AfterResponse fill:#E3F2FD
```

### 2. Skills：自定义能力

```mermaid
flowchart TB
    User[用户] --> Core[核心系统]

    subgraph 技能插件
        S1[代码审查技能]
        S2[写作助手技能]
        S3[数据分析技能]
    end

    Core --> S1
    Core --> S2
    Core --> S3

    S1 --> Core
    S2 --> Core
    S3 --> Core
```

### 3. 插件：外部脚本扩展

```mermaid
flowchart TB
    Gasket[Gasket核心]
    Plugin[插件系统]

    subgraph 外部脚本
        S1[Python 脚本]
        S2[Node.js 脚本]
        S3[Shell 脚本]
    end

    Gasket --> Plugin
    Plugin --> S1
    Plugin --> S2
    Plugin --> S3
```

---

## 部署模式

### 模式1：CLI 交互模式

```mermaid
flowchart LR
    User[用户] --> CLI[gasket agent]
    CLI --> Engine[Engine核心]
    Engine --> LLM
```

### 模式2：Gateway 服务模式

```mermaid
flowchart TB
    subgraph 外部用户
        T[Telegram用户]
        D[Discord用户]
    end

    subgraph Gasket服务
        G[gasket gateway]
        R[Router]
        S1[Session 1]
        S2[Session 2]
    end

    T --> G
    D --> G
    G --> R
    R --> S1
    R --> S2
```

### 模式3：混合模式

```mermaid
flowchart TB
    User[用户] --> Choice{选择?}

    Choice -->|快速任务| CLI[gasket agent]
    Choice -->|长期服务| Gateway[gasket gateway]

    CLI --> Engine
    Gateway --> Engine

    Engine --> LLM
```

---

## 总结

```mermaid
mindmap
  root((Gasket架构))
    用户层
      多渠道接入
      统一消息格式
    接入层
      Router路由
      Session管理
    核心层
      纯函数Kernel
      灵活Hook系统
    能力层
      丰富工具
      长期记忆
      动态技能
    设计哲学
      简洁可预测
      人类友好
      可扩展
```
