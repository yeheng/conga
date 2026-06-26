# 定时任务模块 (Cron)

> 让 AI 自动按时工作的闹钟系统

---

## 一句话理解

Cron 就是 AI 的**闹钟 + 待办事项清单**。到时间了，AI 会自动执行预设的任务。

```mermaid
flowchart LR
    A[设定时间] --> B[到点了] --> C[AI自动执行] --> D[发送结果]
```

---

## 生活中的例子

| 场景 | 类似 Cron 的...
|------|---------------|
| 手机闹钟每天 7 点响 | 每天 7 点发送早安消息 |
| 日历提醒每周一开例会 | 每周一发送周报提醒 |
| 定时器 30 分钟后关火 | 30 分钟后提醒检查任务 |
| 生日提醒每年自动发送祝福 | 每年自动发送祝福消息 |

---

## Cron 能做什么

```mermaid
mindmap
  root((Cron定时任务))
    信息获取
      每天查询天气
      每周获取新闻
      定时检查网站状态
    报告生成
      每日数据汇总
      周报自动生成
      系统健康报告
    维护任务
      自动清理旧文件
      数据库备份
      日志归档
    提醒通知
      会议提醒
      截止日期提醒
      纪念日祝福
```

---

## 定时任务的组成

每个定时任务包含：

```mermaid
flowchart TB
    subgraph 一个定时任务
        A[什么时候执行<br/>Cron表达式] 
        B[做什么<br/>任务内容]
        C[发给谁<br/>目标渠道]
        D[启用状态<br/>开/关]
    end
    
    A --> E[任务执行]
    B --> E
    C --> E
    D --> E
```

### 1. 什么时候执行？（Cron 表达式）

Cron 表达式是一种时间格式，告诉系统**何时**执行任务：

```mermaid
flowchart LR
    subgraph 时间格式
        M[分钟] --> H[小时]
        H --> D[日期]
        D --> Mo[月份]
        Mo --> W[星期]
    end
```

| 表达式 | 含义 | 举例 |
|--------|------|------|
| `0 9 * * *` | 每天 9:00 | 每天上午9点发送早报 |
| `0 */6 * * *` | 每 6 小时 | 每6小时检查一次邮件 |
| `0 9 * * 1` | 每周一 9:00 | 每周一发送周报提醒 |
| `0 0 1 * *` | 每月 1 号 | 每月1号生成月报 |
| `*/5 * * * *` | 每 5 分钟 | 每5分钟检查系统状态 |

### 2. 做什么？（任务内容）

任务内容就是告诉 AI 要执行什么操作：

```mermaid
flowchart TB
    subgraph 任务类型
        A1[让AI思考处理<br/>发送提示词给AI]
        A2[直接执行工具<br/>比如发送邮件]
    end
    
    A1 --> B1[例: 查询今天天气<br/>整理成报告]
    A2 --> B2[例: 执行备份脚本]
```

### 3. 发给谁？（目标渠道）

任务执行结果可以发送到：

```mermaid
flowchart LR
    R[任务结果] --> T[Telegram]
    R --> D[Discord]
    R --> S[Slack]
    R --> W[Webhook]
    R --> L[本地日志]
```

---

## 系统架构

### 文件存储设计

```mermaid
flowchart TB
    subgraph 配置文件
        F1[morning-weather.md]
        F2[daily-report.md]
        F3[weekly-backup.md]
    end
    
    subgraph 每个文件包含
        C1[时间设置<br/>cron: 0 9 * * *]
        C2[任务内容<br/>查询天气并报告]
        C3[目标设置<br/>channel: telegram]
    end
    
    F1 --> C1
    F2 --> C2
    F3 --> C3
```

### 执行流程

```mermaid
sequenceDiagram
    participant Clock as 系统时钟
    participant Cron as Cron服务
    participant File as 任务文件
    participant DB as 状态存储
    participant AI as AI大脑
    participant User as 用户
    
    Note over Cron: 每分钟检查一次
    
    loop 每分钟
        Cron->>File: 读取所有任务配置
        Cron->>DB: 查询上次执行时间
        Cron->>Cron: 计算下次执行时间
        
        alt 到执行时间了
            Cron->>AI: 触发任务执行
            AI->>AI: 处理任务内容
            AI-->>User: 发送结果
            Cron->>DB: 更新执行状态
        else 还没到时间
            Cron->>Cron: 继续等待
        end
    end
```

---

## 混合架构设计

Cron 使用**文件 + 数据库**的混合设计：

```mermaid
flowchart TB
    subgraph 配置层文件
        F[任务定义文件<br/>.md格式]
        F1[人类可编辑]
        F2[版本控制友好]
        F3[热重载支持]
    end
    
    subgraph 状态层数据库
        D[SQLite数据库]
        D1[上次执行时间]
        D2[下次执行时间]
        D3[执行次数统计]
    end
    
    subgraph 内存层运行时
        M[任务调度器]
        M1[缓存任务列表]
        M2[计算执行时间]
        M3[触发执行]
    end
    
    F --> M
    D --> M
    M --> D
```

**为什么这样设计？**
- **文件存配置**：你可以直接编辑文件，用 Git 管理，一目了然
- **数据库存状态**：记录上次执行时间，重启后不会丢失，能检测错过的任务

---

## 实际使用场景

### 场景1：每日天气早报

```mermaid
sequenceDiagram
    participant Time as 每天9:00
    participant Cron as Cron服务
    participant AI as AI大脑
    participant API as 天气API
    participant User as 用户手机
    
    Time->>Cron: 触发任务
    Cron->>AI: 执行: 查询广州天气
    AI->>API: 获取天气数据
    API-->>AI: 返回天气信息
    AI->>AI: 整理成友好格式
    AI-->>User: 发送: 今天广州晴，25°C...
```

**任务文件示例：**
```markdown
---
name: 每日天气
cron: "0 9 * * *"
channel: telegram
to: "用户ID"
---

查询广州今天和未来三天的天气情况，
用亲切的语气发送给用户。
```

### 场景2：系统自动维护

```mermaid
flowchart TB
    subgraph 系统维护任务
        T3[每小时<br/>检查cron配置更新]
    end

    T3 --> C[Cron服务]
```

这些任务**直接执行工具**，不经过 AI，零成本：
- `cron refresh`：重新加载任务配置

### 场景3：错过的任务补执行

```mermaid
sequenceDiagram
    participant System as 系统启动
    participant Cron as Cron服务
    participant Task as 每日备份任务
    participant User as 用户
    
    Note over System: 系统昨晚维护了8小时
    
    System->>Cron: 启动服务
    Cron->>Task: 检查上次执行时间
    Task-->>Cron: 昨天9:00执行过
    Cron->>Cron: 下次应该是今天9:00
    Cron->>Cron: 现在10:00，已经过了！
    Cron->>Task: 立即补执行
    Task->>User: 发送备份完成通知
```

---

## 任务的生命周期

```mermaid
stateDiagram-v2
    [*] --> 创建: 添加任务文件
    创建 --> 启用: 启用任务
    启用 --> 等待: 计算下次执行时间
    等待 --> 执行: 时间到达
    执行 --> 完成: 执行成功
    完成 --> 等待: 计算下次执行时间
    执行 --> 失败: 执行失败
    失败 --> 等待: 记录错误，等待下次
    启用 --> 禁用: 手动禁用
    禁用 --> 启用: 手动启用
    禁用 --> [*]: 删除文件
```

---

## 如何使用

### 1. 查看所有任务

```bash
gasket cron list
```

输出示例：
```
每日天气
  时间: 每天 9:00
  状态: 启用 ✓
  下次: 明天 9:00

周报提醒
  时间: 每周一 9:00
  状态: 启用 ✓
  下次: 下周一 9:00
```

### 2. 添加新任务

```bash
# 命令行方式
gasket cron add "每日天气" "0 9 * * *" "查询广州天气并发送"

# 或者创建文件 ~/.gasket/cron/daily-weather.md
```

### 3. 启用/禁用任务

```bash
gasket cron enable daily-weather   # 启用
gasket cron disable daily-weather  # 禁用
```

### 4. 手动编辑任务文件

直接编辑文件，系统会自动检测变化：

```bash
vim ~/.gasket/cron/daily-weather.md
# 修改后保存，立即生效，无需重启
```

---

## 常见问题

**Q: 如果电脑关机了，错过的任务怎么办？**
A: 系统会记住下次执行时间，开机后会检查是否有错过的任务，并立即补执行。

**Q: 任务文件修改后要重启吗？**
A: 不需要！系统会监控文件变化，保存后立即生效。

**Q: 可以设置多少任务？**
A: 没有限制，但建议合理规划，避免同时执行太多任务。

**Q: 任务执行失败会重试吗？**
A: 每次任务独立执行，失败后记录日志，等待下次执行时间再试。
