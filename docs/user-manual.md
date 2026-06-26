# Gasket 使用手册

> 从零开始，手把手教你使用 Gasket —— 你的私人 AI 助手框架

---

## 这份手册适合谁？

**任何人。** 你不需要会编程，不需要懂 AI，不需要有技术背景。只要你会用电脑，就能跟着这份手册把 Gasket 用起来。

---

## 目录

| 章节 | 内容 | 你会学到 |
|------|------|----------|
| [一、Gasket 是什么](#一gasket-是什么) | 30 秒理解 Gasket | Gasket 能做什么 |
| [二、安装与启动](#二安装与启动) | 从零搭建环境 | 让 Gasket 在你的电脑上跑起来 |
| [三、与 AI 对话](#三与-ai-对话agent-模式) | 最核心的功能 | 和 AI 聊天、提问、让它帮你做事 |
| [四、配置 AI 大脑](#四配置-ai-大脑providers) | 选择不同的 AI 模型 | 用最合适的 AI 模型做最合适的事 |
| [五、多模型切换](#五多模型切换model-profiles) | 定义多套模型方案 | 一句话让 AI 切换"大脑" |
| [六、定时任务 (Cron)](#六定时任务cronai-的闹钟) | AI 的闹钟 | 让 AI 定时自动执行任务 |
| [七、加密保险箱 (Vault)](#七加密保险箱vault) | 安全存储敏感信息 | 安全管理密码、API Key 等 |
| [八、多渠道网关 (Gateway)](#八多渠道网关gateway) | 让 AI 登上各种聊天平台 | Telegram、Discord、Slack 上使用 AI |
| [九、工具系统](#九工具系统ai-的双手) | AI 如何操作电脑 | 文件读写、网页搜索、代码执行 |
| [十、插件系统](#十插件系统扩展-ai-的能力) | 扩展 AI 能力 | 给 AI 添加新技能 |
| [十一、常用命令速查表](#十一常用命令速查表) | 随用随查 | 所有命令一目了然 |
| [十二、常见问题 (FAQ)](#十二常见问题-faq) | 遇到问题怎么办 | 快速解决常见问题 |

---

## 一、Gasket 是什么

### 一句话解释

**Gasket 是一个可以让 AI 帮你干活的工具。** 它就像雇了一个 24 小时在线的私人助理，你可以跟它聊天、让它写代码、管理文件、搜索网页、定时提醒，甚至连接到你的 Telegram / Discord / 飞书 等聊天软件上随时响应。

### 它能做什么？

```
你 ──说话──> Gasket ──转发给──> AI 大脑（如 ChatGPT、Claude 等）
                                      |
                                 AI 思考并回复
                                      |
你 <──看到结果── Gasket <──回复───────+
```

| 能力 | 举例 |
|------|------|
| **聊天问答** | "帮我翻译这段话"、"解释一下量子计算" |
| **写代码** | "用 Python 写一个网页爬虫" |
| **文件操作** | "帮我读取这个日志文件，找出所有错误" |
| **网页搜索** | "搜索最近的 AI 新闻" |
| **长期记忆** | 记住你的项目信息、技术决策，下次自动参考 |
| **定时任务** | 每天早上 9 点自动发送天气预报 |
| **多平台接入** | 在 Telegram、Discord、飞书上都能用 |

### 两种使用方式

| 方式 | 适合场景 | 命令 |
|------|---------|------|
| **命令行 (CLI)** | 自己在终端里用 | `gasket agent` |
| **网关 (Gateway)** | 让其他人通过聊天软件用 | `gasket gateway` |

> 本手册先教你 CLI 方式（最简单），后面再讲 Gateway。

---

## 二、安装与启动

### 第 1 步：安装 Rust 语言环境

Gasket 是用 Rust 语言写的，所以需要先安装 Rust。

```bash
# Mac / Linux 用户 —— 在终端里执行：
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# 安装完成后，让环境变量生效：
source ~/.cargo/env

# 验证安装成功（应该看到版本号，如 1.75.0）：
rustc --version
```

> **Windows 用户**：访问 [rustup.rs](https://rustup.rs) 下载安装程序。推荐使用 WSL2。

### 第 2 步：下载并编译 Gasket

```bash
# 下载代码到本地
git clone https://github.com/YeHeng/gasket.git
cd gasket

# 编译（需要 2-5 分钟，请耐心等待）
cargo build --release

# 安装到系统（这样可以在任何目录使用 gasket 命令）
cargo install --path gasket/cli

# 验证安装成功
gasket --version
# 输出类似：gasket 2.0.0
```

### 第 3 步：初始化工作空间

```bash
# 首次使用，初始化配置
gasket onboard
```

输出示例：
```
🚀 初始化 Gasket 工作空间...

✓ 创建目录: ~/.gasket
✓ 生成配置: ~/.gasket/config.yaml
✓ 创建个人资料: ~/.gasket/PROFILE.md
✓ 创建记忆目录: ~/.gasket/memory/
✓ 创建技能目录: ~/.gasket/skills/

下一步:
1. 编辑 ~/.gasket/config.yaml 填入 API Key
2. 运行: gasket agent
```

> **发生了什么？** Gasket 在你的用户目录下创建了一个 `.gasket/` 文件夹，所有配置和数据都会存在这里。

### 第 4 步：获取 AI 的 API Key

AI 的"大脑"需要通过 API（应用程序接口）来调用。你需要一个 API Key 来访问 AI 服务。

**推荐新手使用 OpenRouter**（聚合多家 AI，新用户有免费额度）：

1. 访问 [openrouter.ai](https://openrouter.ai)
2. 注册账号（支持 Google / GitHub 登录）
3. 点击 "Create API Key"
4. 复制生成的 Key（格式如 `sk-or-v1-xxxxxxxx`）

> **其他选择**：智谱（免费额度多）、DeepSeek（便宜好用）、OpenAI（最强但贵）

### 第 5 步：配置 API Key

用任意文本编辑器打开配置文件：

```bash
# Mac 用户可以用 VS Code：
code ~/.gasket/config.yaml

# 或者用 nano：
nano ~/.gasket/config.yaml
```

找到 `providers` 部分，添加你的 API Key：

```yaml
providers:
  openrouter:
    api_base: "https://openrouter.ai/api/v1"
    api_key: "sk-or-v1-你的Key粘贴在这里"    # <-- 替换为你的真实 Key

# 设置默认使用的 AI 模型
agents:
  defaults:
    model: "openrouter/anthropic/claude-sonnet-4"   # 格式：提供商/模型名
```

保存文件后，验证配置：

```bash
gasket status
# 应该显示你的提供商和 API Key 状态
```

### 第 6 步：开始第一次对话！

```bash
gasket agent
```

你会看到：
```
🤖 Gasket v2.0.0
Model: openrouter/anthropic/claude-sonnet-4

你: 你好！
🤖 Gasket: 你好！很高兴见到你，有什么我可以帮助你的吗？

你: _
```

**恭喜，你已经成功启动了 Gasket！**

---

## 三、与 AI 对话（Agent 模式）

### 3.1 两种对话方式

| 方式 | 命令 | 适合场景 |
|------|------|---------|
| **交互模式** | `gasket agent` | 持续对话，来回聊天 |
| **单次模式** | `gasket agent -m "你的问题"` | 问一个问题就走 |

#### 交互模式示例

```bash
gasket agent

你: 今天天气怎么样？
🤖 Gasket: 我无法实时查询天气，但我可以帮你搜索...

你: 帮我用 Python 写一个猜数字游戏
🤖 Gasket: 好的，这是一个猜数字游戏：
  [AI 会生成完整代码]

你: /new                        # <-- 开启新对话
🤖 Gasket: 已开启新对话，历史已清空。

你: /exit                       # <-- 退出
```

#### 单次模式示例

```bash
# 问一个问题，AI 回复后自动退出
gasket agent -m "什么是 REST API？"

# 开启深度思考模式（适合复杂问题）
gasket agent -m "分析这段代码的性能瓶颈" --thinking

# 禁用 Markdown 格式（纯文本输出）
gasket agent -m "列出 10 个 Python 技巧" --no-markdown
```

### 3.2 交互模式中的命令

在交互模式中，你可以输入以下特殊命令（以 `/` 开头）：

| 命令 | 作用 | 示例 |
|------|------|------|
| `/new` | 开启新对话（清空当前历史） | 聊完一个话题后，输入 `/new` 开始新话题 |
| `/help` | 显示帮助信息 | 忘了怎么用？输入 `/help` |
| `/exit` | 退出程序 | 结束对话 |
| `/quit` | 同上（等同于 `/exit`） | 同上 |
| `:q` | 同上（等同 Vim 快捷键） | 同上 |

### 3.3 启动选项

```bash
# 基础用法
gasket agent                          # 交互模式
gasket agent -m "你好"                # 单次消息

# 高级选项
gasket agent --thinking               # 开启深度思考（适合数学、推理）
gasket agent --logs                   # 显示调试日志（排错用）
gasket agent --no-stream              # 关闭流式输出（等 AI 想完再一次性显示）
gasket agent --no-markdown            # 关闭 Markdown 渲染（纯文本）
```

### 3.4 实用对话场景

**场景 1：让 AI 帮你写一封邮件**

```
你: 帮我写一封请假邮件，理由是家中有事需要处理，请假 3 天，语气正式
🤖 Gasket:
  尊敬的领导：
  您好！因家中有事需要处理，特申请休假3天...
```

**场景 2：让 AI 解释代码**

```
你: 解释一下这段代码：for i in range(len(list)): print(list[i])
🤖 Gasket:
  这段代码的作用是遍历列表并打印每个元素...
  更好的写法是：for item in list: print(item)
```

**场景 3：让 AI 帮你翻译**

```
你: 把以下内容翻译成英文：人工智能正在改变我们的生活方式
🤖 Gasket:
  Artificial intelligence is changing our way of life.
```

---

## 四、配置 AI 大脑（Providers）

Gasket 支持接入多种 AI 服务商。你可以根据需求选择不同的"大脑"。

### 4.1 支持的服务商

| 服务商 | 特点 | 价格 | 推荐场景 |
|--------|------|------|---------|
| **OpenRouter** | 聚合平台，一个 Key 用多家 | 按模型不同 | 新手首选 |
| **智谱 (Zhipu)** | 中文能力强，有免费额度 | 低 | 中文对话 |
| **DeepSeek** | 编程能力强，性价比高 | 低 | 写代码 |
| **OpenAI** | GPT 系列，综合最强 | 中高 | 通用场景 |
| **Anthropic** | Claude 系列，长文理解强 | 中高 | 长文分析 |
| **Gemini** | Google 出品，多模态 | 中 | 图片理解 |
| **Ollama** | 本地运行，完全免费 | 免费 | 隐私敏感 |

### 4.2 配置示例

编辑 `~/.gasket/config.yaml`：

```yaml
providers:
  # ---- 中文场景首选：智谱 ----
  zhipu:
    api_base: "https://open.bigmodel.cn/api/paas/v4"
    api_key: "你的智谱API-Key"

  # ---- 编程场景：DeepSeek ----
  deepseek:
    api_base: "https://api.deepseek.com/v1"
    api_key: "你的DeepSeek-API-Key"
    models:
      deepseek-chat:                    # 普通对话模型
        price_input_per_million: 0.5    # 输入价格（每百万 token）
        price_output_per_million: 1.0   # 输出价格
      deepseek-reasoner:                # 推理模型（深度思考）
        thinking_enabled: true          # 开启思考模式
        max_tokens: 8192                # 最大输出长度

  # ---- 聚合平台：OpenRouter ----
  openrouter:
    api_base: "https://openrouter.ai/api/v1"
    api_key: "你的OpenRouter-API-Key"

  # ---- 本地模型：Ollama（不需要 API Key）----
  ollama:
    api_base: "http://localhost:11434/v1"
    # 无需 api_key，完全本地运行

# 设置默认模型
agents:
  defaults:
    model: "zhipu/glm-5"               # 格式：服务商/模型名
```

### 4.3 使用环境变量保护 API Key

更安全的做法是使用环境变量，避免在配置文件中明文写入 Key：

```yaml
providers:
  openai:
    api_base: "https://api.openai.com/v1"
    api_key: "${OPENAI_API_KEY}"        # 读取环境变量 OPENAI_API_KEY
```

然后在终端设置环境变量：

```bash
# 临时设置（仅当前终端有效）
export OPENAI_API_KEY="sk-xxxxxxxx"

# 永久设置（写入 shell 配置文件）
echo 'export OPENAI_API_KEY="sk-xxxxxxxx"' >> ~/.zshrc
source ~/.zshrc
```

### 4.4 查看当前配置状态

```bash
gasket status
```

输出示例：
```
Providers:
  openrouter: ✓ configured (API key set)
  zhipu:      ✗ not configured (no API key)
  deepseek:   ✓ configured (API key set)

Default model: openrouter/anthropic/claude-sonnet-4
```

---

## 五、多模型切换（Model Profiles）

Gasket 允许你定义多个"模型方案"，让 AI 在不同任务中自动或手动切换不同的"大脑"。

### 5.1 为什么需要多模型？

```
简单问题（"今天星期几？"）   → 用 快速模型（便宜、快）
写代码                      → 用 代码模型（专业、准确）
复杂推理（"证明数学定理"）    → 用 推理模型（深度思考）
创意写作                    → 用 创意模型（灵活、有想象力）
```

### 5.2 配置模型方案

编辑 `~/.gasket/config.yaml`：

```yaml
agents:
  defaults:
    model: "zhipu/glm-5"               # 默认模型

  models:
    # 通用模型 —— 日常对话
    default:
      provider: "zhipu"
      model: "glm-5"
      description: "通用模型，日常对话使用"
      capabilities: ["general", "chat"]
      temperature: 0.7                  # 0=严谨, 1=有创意

    # 快速模型 —— 简单问题
    fast:
      provider: "zhipu"
      model: "glm-4-flash"
      description: "快速响应，适合简单问题"
      capabilities: ["fast", "chat"]
      temperature: 0.7
      max_tokens: 2048                  # 限制输出长度（省钱）

    # 编程模型 —— 写代码
    coder:
      provider: "deepseek"
      model: "deepseek-coder"
      description: "专门用于编程、代码审查和调试"
      capabilities: ["code", "reasoning"]
      temperature: 0.3                  # 编程要严谨，温度调低

    # 推理模型 —— 复杂问题
    reasoner:
      provider: "deepseek"
      model: "deepseek-reasoner"
      description: "深度推理，适合数学、逻辑、分析"
      capabilities: ["reasoning", "math"]
      temperature: 0.7
      thinking_enabled: true            # 开启深度思考模式

    # 创意模型 —— 写作
    creative:
      provider: "anthropic"
      model: "claude-sonnet-4"
      description: "创意写作、内容创作"
      capabilities: ["creative", "writing"]
      temperature: 0.9                  # 提高温度，更有创意

    # 本地模型 —— 隐私敏感
    local:
      provider: "ollama"
      model: "llama3"
      description: "本地运行，数据不出电脑"
      capabilities: ["local", "private"]
```

### 5.3 使用效果

AI 会在对话中根据任务类型自动选择合适的模型。你也可以在对话中这样告诉 AI：

```
你: 用 coder 模型帮我写一个排序算法
🤖 Gasket: [自动切换到 deepseek-coder 模型]
  好的，这是一个快速排序算法的实现...
```

> **temperature 参数解释**：`0` = 每次回答都一样（严谨），`1` = 每次回答可能不同（有创意）。编程用 `0.3`，聊天用 `0.7`，写作用 `0.9`。

---

## 六、定时任务（Cron）—— AI 的闹钟

### 6.1 什么是定时任务？

就像手机上的闹钟和日历提醒，你可以设定时间和任务，让 AI 自动执行。

```
你：每天早上 9 点提醒我开站会
        ↓
Gasket：好的，已设置定时任务
        ↓
[第二天早上 9:00]
Gasket：提醒：现在是站会时间！
```

### 6.2 Cron 表达式（时间格式）

Cron 表达式由 5 个数字组成，分别代表：**分 时 日 月 周**

| 表达式 | 含义 | 生活场景 |
|--------|------|---------|
| `0 9 * * *` | 每天 9:00 | 每天早上 9 点发早报 |
| `0 */6 * * *` | 每 6 小时 | 每 6 小时检查一次系统 |
| `0 9 * * 1` | 每周一 9:00 | 每周一发周报提醒 |
| `0 0 1 * *` | 每月 1 号 0:00 | 每月生成月报 |
| `*/5 * * * *` | 每 5 分钟 | 每 5 分钟检查服务状态 |
| `30 14 25 12 *` | 12 月 25 日 14:30 | 圣诞节下午提醒 |

### 6.3 管理定时任务

```bash
# 添加一个定时任务：每天早上 9 点发送天气预报
gasket cron add \
  --name "每日天气" \
  --cron "0 9 * * *" \
  --message "查询今天的天气预报并发送给我"

# 查看所有定时任务
gasket cron list
```

输出示例：
```
Scheduled Jobs:
  [a3f1b2c4] 每日天气
    Cron: 0 9 * * *
    Status: enabled
    Next run: 2026-04-24 09:00:00

  [e5d6c7b8] 周报提醒
    Cron: 0 9 * * 1
    Status: enabled
    Next run: 2026-04-28 09:00:00
```

```bash
# 查看某个任务的详情（包括未来 5 次执行时间）
gasket cron show a3f1b2c4

# 临时关闭某个任务（不删除）
gasket cron disable a3f1b2c4

# 重新启用
gasket cron enable a3f1b2c4

# 删除任务
gasket cron remove a3f1b2c4
```

### 6.4 更多定时任务示例

```bash
# 每周一早上 9 点提醒写周报
gasket cron add \
  --name "周报提醒" \
  --cron "0 9 * * 1" \
  --message "提醒：今天是周一，请准备周报"

# 每 5 分钟检查网站是否在线
gasket cron add \
  --name "网站监控" \
  --cron "*/5 * * * *" \
  --message "检查 https://example.com 是否在线，如果不可达则通知我"

# 每月 1 号生成月度报告
gasket cron add \
  --name "月度报告" \
  --cron "0 0 1 * *" \
  --message "生成本月的数据统计报告"
```

### 6.5 定时任务文件

定时任务也可以直接编辑 Markdown 文件来管理，文件位于：

```
~/.gasket/cron/
├── daily-weather.md
├── weekly-report.md
└── site-monitor.md
```

每个文件就是一个定时任务的定义。修改后运行 `gasket cron refresh` 让 Gasket 重新加载。

---

## 八、加密保险箱（Vault）

### 8.1 为什么需要 Vault？

你的配置文件中可能有敏感信息（API Key、数据库密码等），直接明文写在文件里不安全。Vault 提供加密存储，只在运行时解密使用。

```
普通存储：  config.yaml 里写着 api_key: "sk-xxxxxxxx"     ← 明文，不安全！
Vault存储：  config.yaml 里写着 api_key: "{{vault:openai}}"  ← 加密占位符，安全！
```

### 8.2 设置主密码

Vault 需要一个主密码来加解密。设置环境变量：

```bash
# 设置主密码（每次使用前都需要）
export GASKET_MASTER_PASSWORD="你的强密码"

# 建议写入 shell 配置文件，省去每次手动设置：
echo 'export GASKET_MASTER_PASSWORD="你的强密码"' >> ~/.zshrc
source ~/.zshrc
```

### 8.3 管理密钥

```bash
# 添加一个密钥（交互式，会提示你输入值）
gasket vault set openai_api_key
# 输入值: sk-xxxxxxxxxxxxxxxx

# 直接设置值和描述
gasket vault set db_password -v "my_secret_password" -d "数据库密码"

# 查看所有密钥（值会被隐藏）
gasket vault list
```

输出示例：
```
Vault Entries:
  openai_api_key  | API key for OpenAI       | Created: 2026-04-23
  db_password     | 数据库密码                | Created: 2026-04-23
```

```bash
# 查看某个密钥的详情（值仍然隐藏）
gasket vault show openai_api_key

# 查看密钥的值（谨慎使用）
gasket vault get openai_api_key
# 输出: sk-xxxxxxxxxxxxxxxx

# 删除密钥（会确认）
gasket vault delete openai_api_key

# 强制删除（不确认）
gasket vault delete openai_api_key --force
```

### 8.4 在配置文件中使用 Vault

设置好密钥后，在 `config.yaml` 中用 `{{vault:密钥名}}` 代替明文：

```yaml
providers:
  openai:
    api_base: "https://api.openai.com/v1"
    api_key: "{{vault:openai_api_key}}"     # 运行时自动替换为真实值

  deepseek:
    api_base: "https://api.deepseek.com/v1"
    api_key: "{{vault:deepseek_api_key}}"
```

### 8.5 导入/导出

```bash
# 导出所有密钥到 JSON 文件（用于备份）
gasket vault export ./vault-backup.json

# 从 JSON 文件导入密钥
gasket vault import ./vault-backup.json

# 导入并合并（不覆盖已有的同名密钥）
gasket vault import ./vault-backup.json --merge
```

---

## 九、多渠道网关（Gateway）

### 9.1 什么是 Gateway？

Gateway 模式让 Gasket 作为一个服务器运行，同时连接多个聊天平台。这样你和你的团队可以在不同平台上使用同一个 AI 助手。

```
                ┌── Telegram ──┐
                │              │
用户 A ────────┤              │
                │   Gasket    │──── AI 大脑
用户 B ────────┤  Gateway     │
                │              │
                ├── Discord ───┘
                │
用户 C ────────┤
                │
                └── Slack ─────┘
```

### 9.2 启动 Gateway

```bash
gasket gateway
```

Gateway 会在后台持续运行，监听所有已配置的聊天渠道。

### 9.3 配置聊天渠道

编辑 `~/.gasket/config.yaml`：

#### Telegram（最常用）

1. 在 Telegram 中找 [@BotFather](https://t.me/BotFather) 创建机器人
2. 获取 Bot Token

```yaml
channels:
  telegram:
    enabled: true
    token: "123456:ABC-DEF..."           # 从 BotFather 获取的 Token
    allow_from: []                        # 空列表 = 允许所有人，或填用户 ID 限制
    # allow_from: [123456789]            # 只允许指定用户
```

#### Discord

1. 在 [Discord Developer Portal](https://discord.com/developers/applications) 创建 Bot
2. 获取 Bot Token

```yaml
channels:
  discord:
    enabled: true
    token: "你的Discord-Bot-Token"
    allow_from: []
```

#### Slack

1. 在 [Slack API](https://api.slack.com/apps) 创建 App
2. 获取 Bot Token（`xoxb-` 开头）和 App Token（`xapp-` 开头）

```yaml
channels:
  slack:
    enabled: true
    bot_token: "xoxb-xxxxx"
    app_token: "xapp-xxxxx"
    allow_from: []
```

#### 飞书 (Feishu)

```yaml
channels:
  feishu:
    enabled: true
    app_id: "cli_xxxxx"
    app_secret: "xxxxx"
    verification_token: "xxxxx"
    encrypt_key: "xxxxx"
    allow_from: []
```

#### 钉钉 (DingTalk)

```yaml
channels:
  dingtalk:
    enabled: true
    webhook_url: "https://oapi.dingtalk.com/robot/send?access_token=xxxxx"
    secret: "SECxxxxx"
    allow_from: []
```

#### 企业微信 (WeCom)

```yaml
channels:
  wecom:
    enabled: true
    corpid: "wwxxxxx"
    corpsecret: "xxxxx"
    agent_id: "1000002"
    token: "xxxxx"
    encoding_aes_key: "xxxxx"
    allow_from: []
```

### 9.4 查看渠道状态

```bash
gasket channels status
```

输出示例：
```
Channel Status:
  telegram:   ✓ connected (Bot: @my_gasket_bot)
  discord:    ✗ not configured
  slack:      ✓ connected (Bot: gasket-assistant)
  websocket:  ✓ listening on port 3000
```

### 9.5 Gateway 独有功能

Gateway 模式下，AI 额外拥有以下能力：

| 能力 | 说明 |
|------|------|
| `send_message` | 主动发送消息给用户 |
| `cron` | 通过对话管理定时任务 |
| `context` | 管理对话上下文（压缩长对话） |

---

## 十、工具系统（AI 的双手）

### 10.1 什么是工具？

AI 不仅能"说话"，还能"做事"。工具就是 AI 操作电脑的方式：

```
你: 帮我读取 config.yaml 文件
   ↓
AI: 我来帮你读取 [调用 read_file 工具]
   ↓
AI: 文件内容如下：...
```

### 10.2 内置工具一览

#### 安全的只读工具（无需确认）

| 工具 | 功能 | 使用场景 |
|------|------|---------|
| `read_file` | 读取文件内容 | "帮我看看这个文件里写了什么" |
| `list_dir` | 列出目录内容 | "看看这个文件夹里有什么文件" |
| `web_fetch` | 获取网页内容 | "帮我读一下这个网页的内容" |
| `web_search` | 搜索网页 | "搜索一下 Rust 语言的最新版本" |
| `history_query` | 查询对话历史 | "我昨天说了什么？" |

#### 需要确认的写入工具

| 工具 | 功能 | 使用场景 |
|------|------|---------|
| `write_file` | 创建或覆盖文件 | "帮我创建一个配置文件" |
| `edit_file` | 编辑现有文件 | "把第 5 行改成 xxx" |
| `exec` | 执行系统命令 | "运行 python script.py" |
| `new_session` | 开启新会话 | "清空历史，重新开始" |
| `clear_session` | 清空当前会话 | "删除这段对话的历史" |

#### 子代理工具

| 工具 | 功能 | 使用场景 |
|------|------|---------|
| `spawn` | 创建单个子代理 | 让 AI 处理一个独立子任务，支持选择模型 |
| `spawn_parallel` | 并行创建多个子代理 | 同时让多个 AI 做不同的事（最多 10 个任务，5 个并发） |

### 10.3 工具使用示例

你不需要手动调用工具 —— 只需用自然语言告诉 AI 你想做什么，它会自动选择合适的工具：

```
你: 帮我看看 ~/.gasket/config.yaml 里配了哪些提供商
🤖 Gasket: [自动调用 read_file 工具]
  你的配置文件中有以下提供商：
  - openrouter (已配置)
  - zhipu (已配置)
```

```
你: 搜索一下 2026 年最流行的 Rust Web 框架
🤖 Gasket: [自动调用 web_search 工具]
  根据搜索结果，2026 年最流行的 Rust Web 框架有：
  1. Axum - 最受欢迎...
  2. Actix-web - 高性能...
```

```
你: 帮我创建一个 hello.py 文件，内容是打印 Hello World
🤖 Gasket: [需要你确认后才会写入]
  我要创建文件 hello.py，内容如下：
  print("Hello World")
  确认吗？(y/n)
你: y
🤖 Gasket: 文件已创建。
```

### 10.4 配置工具行为

编辑 `~/.gasket/config.yaml`：

```yaml
tools:
  restrict_to_workspace: false            # true = 只允许操作工作空间目录

  web:
    search_provider: brave                 # 搜索引擎：brave / tavily / exa / firecrawl
    brave_api_key: "你的Brave-Search-API-Key"
    # tavily_api_key: "your-key"
    # exa_api_key: "your-key"
    # firecrawl_api_key: "your-key"
    # http_proxy: "http://proxy:8080"     # HTTP 代理（如需要）
    # https_proxy: "http://proxy:8080"
    # socks5_proxy: "socks5://proxy:1080"
    use_env_proxy: true                    # 自动读取 HTTP_PROXY 等环境变量

  exec:
    timeout: 120                           # 命令超时时间（秒）
    workspace: "."                         # 命令执行工作目录

    # 沙箱配置（可选）
    sandbox:
      enabled: false
      backend: bwrap
      tmp_size_mb: 64

    # 命令策略
    policy:
      allowlist: []                        # 允许的命令，空数组 = 允许所有
      denylist: ["rm -rf /", "mkfs"]       # 拒绝的命令模式

    # 资源限制
    limits:
      max_memory_mb: 512                   # 最大内存使用
      max_cpu_secs: 60                     # 最大 CPU 时间
      max_output_bytes: 1048576            # 最大输出大小（1MB）
```

### 10.5 直接执行工具（高级）

不经过 AI 对话，直接执行某个工具：

```bash
# 直接执行 evolution 工具
gasket tool execute evolution '{"threshold": 20}'
```

---

## 十一、插件系统（扩展 AI 的能力）

### 11.1 什么是插件？

插件让你可以用外部脚本来扩展 AI 的能力。比如：

- 调用一个 Python 脚本做数据分析
- 调用一个 Shell 脚本做系统运维
- 连接一个外部 API 获取信息

### 11.2 插件的工作方式

```
用户提问 → AI 决定使用插件 → 调用插件脚本 → 获取结果 → 回复用户
```

### 11.3 创建一个简单插件

在 `~/.gasket/plugins/` 目录下创建一个 YAML 文件：

```bash
mkdir -p ~/.gasket/plugins
```

创建 `~/.gasket/plugins/echo.yaml`：

```yaml
name: echo
description: "回显用户输入的文本"
version: "1.0"
protocol: simple                    # simple = 一次性执行
runtime:
  command: "echo"                   # 要执行的命令
  args: []                          # 命令参数
  timeout_secs: 10                  # 超时时间
parameters:                         # 参数定义（JSON Schema 格式）
  type: object
  properties:
    text:
      type: string
      description: "要回显的文本"
  required: [text]
```

### 11.4 创建一个 Python 插件

创建 `~/.gasket/plugins/word-count.yaml`：

```yaml
name: word_count
description: "统计文本中的单词数量"
version: "1.0"
protocol: simple
runtime:
  command: "python3"
  args: ["/path/to/word_count.py"]
  timeout_secs: 30
parameters:
  type: object
  properties:
    text:
      type: string
      description: "要统计的文本"
  required: [text]
```

创建 `/path/to/word_count.py`：

```python
import json
import sys

# 从标准输入读取 JSON 参数
data = json.loads(sys.stdin.read())
text = data.get("text", "")

# 统计单词数
word_count = len(text.split())

# 输出 JSON 结果
result = {"word_count": word_count}
print(json.dumps(result))
```

### 11.5 JSON-RPC 插件（双向通信）

对于需要持续运行的复杂插件，使用 `json-rpc` 协议：

```yaml
name: my_service
description: "持续运行的后台服务插件"
version: "1.0"
protocol: json-rpc                   # json-rpc = 双向通信
runtime:
  command: "python3"
  args: ["-u", "/path/to/my_service.py"]
  timeout_secs: 300
parameters:
  type: object
  properties:
    action:
      type: string
      description: "要执行的操作"
  required: [action]
```

---

## 十二、常用命令速查表

### 全局命令

| 命令 | 功能 |
|------|------|
| `gasket onboard` | 首次初始化 |
| `gasket status` | 查看当前配置状态 |
| `gasket --version` | 查看版本号 |

### Agent（对话）

| 命令 | 功能 |
|------|------|
| `gasket agent` | 启动交互对话 |
| `gasket agent -m "问题"` | 单次提问 |
| `gasket agent --thinking` | 开启深度思考 |
| `gasket agent --logs` | 显示调试日志 |
| `gasket agent --no-stream` | 关闭流式输出 |
| `gasket agent --no-markdown` | 纯文本输出 |

### Cron（定时任务）

| 命令 | 功能 |
|------|------|
| `gasket cron list` | 查看所有任务 |
| `gasket cron add --name 名字 --cron 表达式 --message 内容` | 添加任务 |
| `gasket cron show <ID>` | 查看任务详情 |
| `gasket cron enable <ID>` | 启用任务 |
| `gasket cron disable <ID>` | 禁用任务 |
| `gasket cron remove <ID>` | 删除任务 |
| `gasket cron refresh` | 重新加载任务文件 |

### Vault（加密保险箱）

| 命令 | 功能 |
|------|------|
| `gasket vault list` | 列出所有密钥（值隐藏） |
| `gasket vault set <KEY>` | 添加/更新密钥 |
| `gasket vault get <KEY>` | 查看密钥值 |
| `gasket vault show <KEY>` | 查看密钥详情 |
| `gasket vault delete <KEY>` | 删除密钥 |
| `gasket vault export <文件>` | 导出到 JSON |
| `gasket vault import <文件>` | 从 JSON 导入 |

### Gateway（网关）

| 命令 | 功能 |
|------|------|
| `gasket gateway` | 启动网关服务器 |
| `gasket channels status` | 查看渠道连接状态 |

### Tool（工具）

| 命令 | 功能 |
|------|------|
| `gasket tool execute <名称> '<JSON参数>'` | 直接执行工具 |

### Auth（认证）

| 命令 | 功能 |
|------|------|
| `gasket auth copilot` | GitHub Copilot 认证 |
| `gasket auth status` | 查看认证状态 |

---

## 十三、常见问题 (FAQ)

### 安装相关

**Q: 编译失败怎么办？**

确保 Rust 版本够新：
```bash
rustc --version        # 需要 >= 1.75
rustup update          # 更新 Rust
```

Mac 用户需要 Xcode 命令行工具：
```bash
xcode-select --install
```

Linux 用户需要安装依赖：
```bash
# Ubuntu / Debian
sudo apt-get install build-essential pkg-config libssl-dev

# Fedora / RHEL
sudo dnf install gcc openssl-devel
```

**Q: 找不到 `gasket` 命令？**

```bash
# 确保 Cargo bin 目录在 PATH 中
export PATH="$HOME/.cargo/bin:$PATH"

# 永久生效
echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.zshrc
source ~/.zshrc
```

### API 和模型相关

**Q: 用什么模型好？**

| 场景 | 推荐模型 |
|------|---------|
| 中文聊天 | 智谱 GLM-5（便宜、中文好） |
| 写代码 | DeepSeek-Coder（便宜、专业） |
| 通用 | Claude Sonnet（全面） |
| 复杂推理 | DeepSeek-Reasoner（深度思考） |
| 隐私敏感 | Ollama 本地模型（免费） |

**Q: 如何省钱？**

1. 选择便宜的模型（DeepSeek、智谱 的价格是 Claude 的 1/10）
2. 限制 `max_tokens`（减少输出长度）
3. 使用本地模型（Ollama，完全免费）

```yaml
agents:
  defaults:
    model: "zhipu/glm-4-flash"     # 便宜
    max_tokens: 2000               # 限制输出
```

**Q: API Key 怎么放最安全？**

方案一：环境变量
```bash
export OPENAI_API_KEY="sk-xxxxx"
```

方案二：Vault 加密存储
```bash
gasket vault set openai_api_key
# 然后在配置中用 {{vault:openai_api_key}}
```

### 使用相关

**Q: AI 回复时乱码或不完整？**

- 检查终端是否支持 UTF-8
- 尝试 `gasket agent --no-markdown` 关闭 Markdown 渲染

**Q: 对话太长导致 AI "忘记"前面的内容？**

这是 AI 上下文窗口的限制。解决方法：
1. 输入 `/new` 开始新对话

**Q: 如何查看使用了多少 Token？**

```bash
gasket stats
```

### Gateway 相关

**Q: Telegram Bot 怎么创建？**

1. 在 Telegram 中搜索 `@BotFather`
2. 发送 `/newbot`
3. 按提示设置名称
4. 获得 Bot Token
5. 填入 `config.yaml`

**Q: Gateway 启动后 Telegram 没响应？**

1. 检查 `gasket channels status` 看连接状态
2. 检查 Token 是否正确
3. 检查网络是否能访问 Telegram API（可能需要代理）

```yaml
tools:
  web:
    https_proxy: "http://your-proxy:8080"
```

---

## 附录：配置文件完整模板

以下是一个包含所有常用配置的完整模板，你可以根据自己的需求修改：

```yaml
# ~/.gasket/config.yaml 完整配置模板

# ==================== AI 服务商 ====================
providers:
  # 智谱（中文能力强）
  zhipu:
    api_base: "https://open.bigmodel.cn/api/paas/v4"
    api_key: "${ZHIPU_API_KEY}"

  # DeepSeek（编程能力强）
  deepseek:
    api_base: "https://api.deepseek.com/v1"
    api_key: "${DEEPSEEK_API_KEY}"
    models:
      deepseek-chat:
        price_input_per_million: 0.5
        price_output_per_million: 1.0
      deepseek-reasoner:
        thinking_enabled: true
        max_tokens: 8192

  # OpenRouter（聚合平台）
  openrouter:
    api_base: "https://openrouter.ai/api/v1"
    api_key: "${OPENROUTER_API_KEY}"

  # Ollama（本地模型，无需 API Key）
  ollama:
    api_base: "http://localhost:11434/v1"

# ==================== AI 行为配置 ====================
agents:
  defaults:
    model: "zhipu/glm-5"
    max_iterations: 20
    temperature: 0.7
    max_tokens: 4096
    memory_window: 10
    streaming: true

  models:
    default:
      provider: "zhipu"
      model: "glm-5"
      description: "通用模型"
      capabilities: ["general"]
      temperature: 0.7

    coder:
      provider: "deepseek"
      model: "deepseek-coder"
      description: "编程专家"
      capabilities: ["code"]
      temperature: 0.3

    reasoner:
      provider: "deepseek"
      model: "deepseek-reasoner"
      description: "深度推理"
      capabilities: ["reasoning"]
      thinking_enabled: true

    local:
      provider: "ollama"
      model: "llama3"
      description: "本地隐私模型"
      capabilities: ["local"]

# ==================== 聊天渠道 ====================
channels:
  telegram:
    enabled: false
    token: ""
    allow_from: []

  discord:
    enabled: false
    token: ""
    allow_from: []

  websocket:
    enabled: true

# ==================== 工具配置 ====================
tools:
  restrict_to_workspace: false
  web:
    user_agent: "Mozilla/5.0"
    timeout_seconds: 30
  exec:
    enabled: true
    timeout_seconds: 60

# ==================== 向量嵌入（语义搜索）====================
embedding:
  enabled: false
  model: "AllMiniLML6V2"
```

---

> **祝你使用愉快！** 如果遇到问题，查看 [FAQ](#十三常见问题-faq) 或提交 [Issue](https://github.com/YeHeng/gasket/issues)。
