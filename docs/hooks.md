# 钩子系统 (Hooks)

> 在关键时刻插一脚——扩展 AI 行为的插件系统

---

## 一句话理解

Hooks 是 AI 对话流程中的**检查站**，可以在关键时刻插入自定义逻辑：修改输入、拦截请求、记录日志、发送通知...

```mermaid
flowchart LR
    A[用户输入] --> B[检查站1] --> C[检查站2] --> D[AI处理] --> E[检查站3] --> F[返回用户]
```

---

## 生活中的类比

想象一个工厂的生产线：

```mermaid
flowchart TB
    subgraph 工厂生产线
        A[原料进入] --> B{质检站}
        B -->|合格| C[加工]
        B -->|不合格| X[退回]
        C --> D{安全站}
        D --> E[包装]
        E --> F{出库检查}
        F --> G[发货]
    end
    
    style B fill:#FFD700
    style D fill:#FFD700
    style F fill:#FFD700
```

| 工厂检查站 | Hooks 对应 |
|-----------|-----------|
| 原料质检 | 检查用户输入是否合法 |
| 安全监控 | 敏感信息处理 |
| 出库检查 | 记录日志、发送通知 |

---

## 五个关键时刻（Hook 点）

```mermaid
flowchart TB
    Start([开始]) --> A
    
    A[用户输入] --> B{BeforeRequest<br/>请求前}
    B -->|可修改/中止| C[保存用户消息]
    
    C --> D[加载历史] --> E{AfterHistory<br/>历史后}
    E -->|可添加上下文| F[加载记忆]
    
    F --> G[组装提示] --> H{BeforeLLM<br/>发送前}
    H -->|最后修改机会| I[AI处理]
    
    I --> J{AfterToolCall<br/>工具后}
    J -->|只读审计| K[生成回复]
    
    K --> L[保存回复] --> M{AfterResponse<br/>响应后}
    M -->|只读审计| End([结束])
    
    style B fill:#90EE90
    style E fill:#90EE90
    style H fill:#90EE90
    style J fill:#E1BEE7
    style M fill:#E1BEE7
```

### Hook 点详解

| Hook 点 | 时机 | 能做什么 | 执行方式 |
|---------|------|---------|---------|
| **BeforeRequest** | 收到请求后 | 修改输入、拦截请求 | 顺序执行 |
| **AfterHistory** | 加载历史后 | 添加上下文 | 顺序执行 |
| **BeforeLLM** | 发送给AI前 | 最后修改（如密钥注入） | 顺序执行 |
| **AfterToolCall** | 工具调用后 | 审计、记录 | 并行执行 |
| **AfterResponse** | 生成回复后 | 日志、通知 | 并行执行 |

```mermaid
graph TB
    subgraph 顺序执行<br/>可修改上下文
        S1[BeforeRequest]
        S2[AfterHistory]
        S3[BeforeLLM]
    end
    
    subgraph 并行执行<br/>只读审计
        P1[AfterToolCall]
        P2[AfterResponse]
    end
    
    S1 -->|可修改/中止| Next1[下一步]
    S2 -->|可修改| Next2[下一步]
    S3 -->|可修改| Next3[下一步]
    
    P1 -.->|并行触发| Audit1[审计日志]
    P1 -.->|并行触发| Notify1[发送通知]
    
    P2 -.->|并行触发| Audit2[审计日志]
    P2 -.->|并行触发| Notify2[发送消息]
    
    style S1 fill:#C8E6C9
    style S2 fill:#C8E6C9
    style S3 fill:#C8E6C9
    style P1 fill:#E1BEE7
    style P2 fill:#E1BEE7
```

---

## 钩子能做什么

### 1. 修改输入（BeforeRequest）

```mermaid
sequenceDiagram
    participant U as 用户
    participant H as Hook
    participant S as Session
    participant AI as AI
    
    U->>S: 发送消息: "{{vault:密码}}"
    S->>H: BeforeRequest钩子
    
    alt 敏感词过滤
        H->>H: 检测敏感词
        H-->>S: 中止: 包含敏感词
        S-->>U: 提示: 消息包含敏感词
    else 继续处理
        H-->>S: 继续
        S->>AI: 正常处理
    end
```

**应用场景：**
- 敏感词过滤
- 输入格式化
- 权限检查

### 2. 密钥注入（BeforeLLM）

```mermaid
sequenceDiagram
    participant S as Session
    participant H as VaultHook
    participant V as 密钥库
    participant AI as AI模型
    
    S->>H: BeforeLLM钩子
    Note over H: 发现 {{vault:api_key}}
    
    H->>V: 查询 api_key
    V-->>H: 返回 sk-abc123
    
    H->>H: 替换占位符
    Note over H: "{{vault:api_key}}" → "sk-abc123"
    
    H-->>S: 继续，已替换
    S->>AI: 发送含真实密钥的消息
    
    Note over AI: AI看到真实密钥<br/>但历史记录中仍是占位符
```

**应用场景：**
- API 密钥注入
- 数据库密码注入
- 任何敏感数据替换

### 3. 审计日志（AfterResponse）

```mermaid
sequenceDiagram
    participant AI as AI
    participant S as Session
    participant H as 审计Hook
    participant Log as 日志系统
    participant Admin as 管理员
    
    AI-->>S: 生成回复
    S->>S: 保存回复
    
    par 并行执行
        S->>H: AfterResponse钩子
        H->>Log: 记录对话
        Log->>Log: 用户、时间、内容
    and
        S->>U: 返回回复
    end
    
    Admin->>Log: 查看审计日志
```

**应用场景：**
- 记录所有对话
- 合规审计
- 安全监控

### 4. 发送通知（AfterResponse）

```mermaid
sequenceDiagram
    participant U as 用户
    participant S as Session
    participant H as 通知Hook
    participant Push as 推送服务
    participant Phone as 用户手机
    
    U->>S: 提问
    S->>AI: 处理
    AI-->>S: 生成回复
    
    par 返回结果 + 发送通知
        S-->>U: 显示回复
    and
        S->>H: AfterResponse钩子
        H->>H: 判断需要通知？
        H->>Push: 发送推送
        Push->>Phone: 手机通知
    end
```

**应用场景：**
- 长任务完成通知
- 重要消息提醒
- 多渠道同步

---

## 钩子的执行策略

### 顺序执行（可修改）

```mermaid
flowchart LR
    A[Hook1] --> B[Hook2] --> C[Hook3] --> D[继续]
    
    style A fill:#C8E6C9
    style B fill:#C8E6C9
    style C fill:#C8E6C9
```

- 一个接着一个执行
- 可以修改上下文
- 可以中止流程
- 用于：BeforeRequest、AfterHistory、BeforeLLM

### 并行执行（只读）

```mermaid
flowchart TB
    A[触发] --> B[Hook1]
    A --> C[Hook2]
    A --> D[Hook3]
    B --> E[等待全部完成]
    C --> E
    D --> E
    E --> F[继续]
    
    style B fill:#E1BEE7
    style C fill:#E1BEE7
    style D fill:#E1BEE7
```

- 同时执行多个钩子
- 只能读取，不能修改
- 不阻塞主流程
- 用于：AfterToolCall、AfterResponse

---

## 钩子的动作

```mermaid
flowchart TB
    subgraph 钩子执行
        A[执行钩子]
    end
    
    subgraph 可能的结果
        C[Continue继续]
        B[Abort中止]
    end
    
    subgraph 后续
        Next[继续流程]
        Stop[停止处理]
    end
    
    A --> C
    A --> B
    
    C --> Next
    B --> Stop
    
    style C fill:#C8E6C9
    style B fill:#FFCDD2
```

| 动作 | 说明 | 使用场景 |
|------|------|---------|
| **Continue** | 继续执行 | 一切正常 |
| **Abort** | 中止流程 | 非法输入、权限不足 |

---

## 内置钩子实现

### VaultHook - 密钥注入

```mermaid
flowchart TB
    Input[用户消息含<br/>{{vault:api_key}}]
    
    subgraph VaultHook
        Scan[扫描占位符]
        Query[查询密钥库]
        Replace[替换为真实值]
        Record[记录使用的值]
    end
    
    Output[发送给AI<br/>含真实密钥]
    History[保存到历史<br/>仍是占位符]
    
    Input --> Scan
    Scan --> Query
    Query --> Replace
    Replace --> Record
    Record --> Output
    Record --> History
    
    style Output fill:#C8E6C9
    style History fill:#E3F2FD
```

**作用**：在最后一刻把 `{{vault:api_key}}` 替换成真实的密钥，AI 能正常使用，但历史记录里保存的还是占位符，保护敏感信息。

### ExternalShellHook - 外部脚本

```mermaid
sequenceDiagram
    participant S as Session
    participant H as ShellHook
    participant Script as 外部脚本
    participant Log as 日志
    
    S->>H: 触发钩子
    H->>Script: 调用脚本<br/>stdin: 上下文JSON
    Script->>Script: 处理...
    Script-->>H: stdout: 结果JSON
    
    alt 修改了内容
        H->>H: 应用修改
    else 要求中止
        H-->>S: 中止流程
    else 仅审计
        H->>Log: 记录信息
        H-->>S: 继续
    end
```

**作用**：调用外部脚本（Shell/Python 等），让开发者能用任何语言扩展功能。

**限制**：
- 超时：2 秒
- stdout 上限：1MB

---

## 钩子注册表

```mermaid
flowchart TB
    subgraph 钩子注册表
        R[Registry]
        
        R --> B[BeforeRequest队列]
        R --> A[AfterHistory队列]
        R --> L[BeforeLLM队列]
        R --> T[AfterToolCall队列]
        R --> P[AfterResponse队列]
    end
    
    subgraph 各种钩子
        B1[敏感词过滤]
        B2[输入格式化]
        
        L1[Vault注入]
        
        T1[工具审计]
        
        P1[日志记录]
        P2[发送通知]
    end
    
    B --> B1
    B --> B2
    L --> L1
    T --> T1
    P --> P1
    P --> P2
```

---

## 完整流程示例

```mermaid
sequenceDiagram
    participant U as 用户
    participant B as BeforeRequest
    participant A as AfterHistory
    participant L as BeforeLLM
    participant AI as AI大脑
    participant P as AfterResponse
    participant Log as 日志
    participant V as 密钥库
    
    U->>B: 发送: "查天气，密钥{{vault:key}}"
    
    Note over B: 顺序执行
    B->>B: 敏感词检查 ✓
    B->>A: 继续
    
    A->>A: 加载历史
    A->>L: 继续
    
    Note over L: 顺序执行
    L->>V: 查询 vault:key
    V-->>L: 返回真实密钥
    L->>L: 替换占位符
    L->>AI: 继续（含真实密钥）
    
    Note over AI: AI处理...
    AI-->>L: 生成回复
    
    L->>P: 继续
    
    Note over P: 并行执行
    par
        P->>Log: 记录审计日志
    and
        P->>U: 返回回复
    end
```

---

## 如何使用钩子

### 1. 配置 Vault 注入

创建 `~/.gasket/vault/secrets.json`：
```json
{
  "api_key": "sk-abc123",
  "db_password": "secret456"
}
```

使用时在消息中写：
```
请用 {{vault:api_key}} 调用 API
```

### 2. 添加外部脚本钩子

创建 `~/.gasket/hooks/pre_request.sh`：
```bash
#!/bin/bash
# 读取输入JSON
read -r input

# 检查敏感词
if echo "$input" | grep -q "敏感词"; then
    echo '{"abort": true, "message": "包含敏感词"}'
else
    echo '{"abort": false}'
fi
```

### 3. 编程方式注册钩子

> **注意**: 钩子通过 `AgentConfig` 配置，不再直接传递给 `AgentSession`。

```rust
// 通过 AgentConfig 配置钩子
let config = AgentConfig {
    evolution: Some(EvolutionConfig::default()),  // 启用 EvolutionHook
    ..Default::default()
};

// Session 内部自动构建钩子
let session = AgentSession::new(provider, workspace, config, tools).await?;
```

---

## 常见问题

**Q: 钩子和工具有什么区别？**
A: 钩子是在特定时机自动触发的，用户无感知；工具是 AI 主动调用的，需要 AI 决定使用哪个工具。

**Q: 钩子执行失败会怎样？**
A: 顺序执行的钩子失败会中断流程；并行执行的钩子失败会被忽略，不影响主流程。

**Q: 可以有多少个钩子？**
A: 没有限制，每个 Hook 点可以有多个钩子，按注册顺序执行。

**Q: 钩子能访问哪些数据？**
A: 可以访问当前会话的所有上下文：用户输入、历史消息、工具调用、Token 使用等。

**Q: VaultHook 安全吗？**
A: 密钥只在发送给 AI 前一刻注入，历史记录中保存的是占位符，不会泄露真实密钥。
