# 插件系统

> 用外部脚本扩展 Gasket —— 用 Python、Node.js 或任何能输出 JSON 的语言编写自定义工具。

---

## 什么是插件？

Gasket 的插件系统让你无需编写 Rust 就能添加自定义工具。只要用你熟悉的语言写一段脚本，配上一个 YAML 声明文件，放到 `~/.gasket/plugins/` 目录下，Gasket 启动时就会自动发现它，并将其作为原生工具暴露给 AI。

插件支持两种通信协议：

- **Simple** —— 一次性执行：JSON 进，JSON 出（类似普通命令行工具）
- **JSON-RPC** —— 双向通信：脚本执行过程中可以回调 Gasket 的能力（LLM、记忆、子代理等）

---

## 快速示例

```yaml
# ~/.gasket/plugins/weather.yaml
name: "weather"
description: "获取指定城市的当前天气"
version: "1.0.0"
runtime:
  command: "python3"
  args: ["weather.py"]
  timeout_secs: 30
parameters:
  type: object
  properties:
    city:
      type: string
      description: "城市名称"
  required: ["city"]
```

```python
# ~/.gasket/plugins/weather.py
import json, sys

args = json.load(sys.stdin)
city = args["city"]

# ... 获取天气 ...
result = {"temperature": 24, "condition": "sunny"}
print(json.dumps(result))
```

完成。Gasket 现在会把 `weather` 作为一个可用工具提供给 AI。

---

## 目录结构

```
~/.gasket/plugins/
├── weather.yaml
├── weather.py
├── translate.yaml
└── translate.js
```

Gasket 在启动时会扫描 `~/.gasket/plugins/`，将每个 `.yaml` / `.yml` 文件当作插件清单加载。

---

## 清单格式

| 字段 | 类型 | 必填 | 默认值 | 说明 |
|------|------|------|--------|------|
| `name` | 字符串 | 是 | — | 唯一的工具名称（不能和内置工具冲突） |
| `description` | 字符串 | 是 | — | 工具功能描述（AI 会看到这个说明） |
| `version` | 字符串 | 否 | `""` | 版本号 |
| `runtime` | 对象 | 是 | — | 脚本运行方式 |
| `protocol` | 字符串 | 否 | `simple` | `simple` 或 `json_rpc` |
| `parameters` | JSON Schema | 是 | — | AI 调用时需要传入的参数 |
| `permissions` | 列表 | 否 | `[]` | 授予脚本的能力（见下文） |

### `runtime`

| 字段 | 类型 | 默认值 | 说明 |
|------|------|--------|------|
| `command` | 字符串 | — | 可执行文件（如 `python3`、`node`、`bash`） |
| `args` | 列表 | `[]` | 命令行参数 |
| `working_dir` | 字符串 | `.` | 相对于清单文件的工作目录 |
| `timeout_secs` | 整数 | `120` | 最大执行时间（秒） |
| `env` | 字典 | `{}` | 环境变量 |

---

## Simple 协议

默认协议。Gasket 启动子进程，将参数以单行 JSON 写入 **stdin**，等待进程退出，然后从 **stdout** 读取单行 JSON 作为结果。

```
Gasket  → stdin:  {"city":"东京"}\n
脚本    → stdout: {"temperature":18,"condition":"cloudy"}\n
脚本以退出码 0 结束
```

要求：
- stdout 必须输出恰好一行的合法 JSON
- 进程退出码必须为 0
- stderr 会被收集并作为调试信息返回

---

## JSON-RPC 协议

当脚本在执行过程中需要调用 Gasket 的能力时使用 `protocol: json_rpc` —— 例如请求 LLM、搜索记忆或创建子代理。

通信方式是在 stdin/stdout 上进行 **以换行分隔的 JSON-RPC 2.0**。

### 生命周期

1. Gasket 启动子进程
2. Gasket 发送 `initialize` 请求（`id: 0`），携带工具参数
3. 脚本可以随时向 Gasket 发送**请求**（如 `llm/chat`），并读取**响应**
4. 脚本处理完毕后，回复 `initialize` 请求，返回最终结果
5. Gasket 读取结果，进程退出

```json
// Gasket → 脚本（初始化）
{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"city":"东京"}}

// 脚本 → Gasket（请求 LLM）
{"jsonrpc":"2.0","id":1,"method":"llm/chat","params":{"messages":[{"role":"user","content":"总结东京天气"}]}}

// Gasket → 脚本（LLM 响应）
{"jsonrpc":"2.0","id":1,"result":{"content":"今天多云..."}}

// 脚本 → Gasket（最终结果）
{"jsonrpc":"2.0","id":0,"result":{"summary":"今天多云，最高气温 18°C。"}}
```

> **注意：** `id: 0` 被保留给 `initialize` 请求。脚本自己发起的请求应使用 `id >= 1`。

### Daemon 模式（持久进程）

JSON-RPC 插件在底层使用持久守护进程。进程在多次工具调用之间保持存活，避免每次冷启动。如果空闲时间超过 `timeout_secs`，下次调用时会自动重启。

---

## 权限系统

JSON-RPC 插件必须显式声明所需的引擎能力。默认策略是**全部拒绝**。

| 权限 | RPC 方法 | 功能 |
|------|----------|------|
| `llm_chat` | `llm/chat` | 调用 LLM 提供商（chat 接口） |
| `subagent_spawn` | `subagent/spawn` | 创建子代理处理任务 |
| `message_send` | `message/send` | 发送消息到渠道 |
| `user_ask` | `user/ask` | 向用户提问 |

清单示例：

```yaml
permissions:
  - llm_chat
  - subagent_spawn
```

如果脚本调用了未授权的 RPC 方法，Gasket 会返回：

```json
{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"Permission denied: llm/chat"}}
```

### `llm/chat` 参数

```json
{
  "messages": [
    {"role": "system", "content": "你是一个乐于助人的助手"},
    {"role": "user", "content": "你好"}
  ],
  "model": "可选的模型 ID"
}
```

返回：

```json
{
  "content": "你好！有什么可以帮你的？",
  "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15}
}
```

### `memory/search` 参数

```json
{
  "query": "项目截止日期",
  "limit": 5
}
```

### `memory/write` 参数

```json
{
  "content": "用户喜欢深色模式",
  "category": "preference"
}
```

### `subagent/spawn` 参数

```json
{
  "task": "重构这个 Python 函数",
  "model": "可选的模型 ID"
}
```

返回：

```json
{
  "id": "sub-123",
  "response": {"content": "这是重构后的代码..."}
}
```

---

## 错误码

使用标准 JSON-RPC 错误码：

| 错误码 | 含义 |
|--------|------|
| `-32700` | 解析错误 |
| `-32600` | 非法请求 |
| `-32601` | 方法不存在 |
| `-32602` | 参数非法 |
| `-32603` | 内部错误 |
| `-32000` | 权限不足 |

---

## Python JSON-RPC 示例

```python
#!/usr/bin/env python3
import sys, json

def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()

def read_line():
    return json.loads(sys.stdin.readline())

while True:
    req = read_line()
    method = req.get("method")
    req_id = req.get("id")

    if method == "initialize":
        city = req["params"]["city"]
        # 请求 LLM 帮助
        send({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "llm/chat",
            "params": {
                "messages": [{"role": "user", "content": f"总结{city}的天气"}]
            }
        })
        llm_resp = read_line()
        summary = llm_resp.get("result", {}).get("content", "无响应")
        send({"jsonrpc": "2.0", "id": req_id, "result": {"summary": summary}})
    else:
        send({"jsonrpc": "2.0", "id": req_id, "error": {"code": -32601, "message": f"未知方法: {method}"}})
```

---

## 安全说明

- 插件以子进程形式运行，拥有与 Gasket 相同的操作系统权限
- 默认没有沙箱（如有需要，请使用操作系统层面的沙箱工具）
- JSON-RPC 回调严格受权限列表控制
- 不符合 JSON-RPC 格式的 stdout 输出会被记录并丢弃
- 单条消息大小限制为 1 MiB，防止恶意脚本耗尽内存

---

## 相关文档

- [模块详解](modules.md) —— 内部模块设计
- [工具系统](tools.md) —— Gasket 原生工具的工作原理
- [钩子系统](hooks.md) —— 另一种扩展 Gasket 的方式
