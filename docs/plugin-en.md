# Plugin System

> Extend Gasket with external scripts — write tools in Python, Node.js, or any language that speaks JSON.

---

## What is a Plugin?

Gasket's plugin system lets you add custom tools without writing Rust. You write a script in your favorite language, declare a YAML manifest, and drop it into `~/.gasket/plugins/`. Gasket automatically discovers it and exposes it as a native tool.

Plugins support two communication protocols:

- **Simple** — one-shot JSON in, JSON out (like a CLI tool)
- **JSON-RPC** — bidirectional conversation where the script can call back into Gasket (LLM, memory, subagents)

---

## Quick Example

```yaml
# ~/.gasket/plugins/weather.yaml
name: "weather"
description: "Get current weather for a city"
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
      description: "City name"
  required: ["city"]
```

```python
# ~/.gasket/plugins/weather.py
import json, sys

args = json.load(sys.stdin)
city = args["city"]

# ... fetch weather ...
result = {"temperature": 24, "condition": "sunny"}
print(json.dumps(result))
```

That's it. Gasket will now offer `weather` as a tool to the AI.

---

## Directory Layout

```
~/.gasket/plugins/
├── weather.yaml
├── weather.py
├── translate.yaml
└── translate.js
```

Gasket scans `~/.gasket/plugins/` at startup and loads every `.yaml` / `.yml` file as a plugin manifest.

---

## Manifest Format

| Field | Type | Required | Default | Description |
|-------|------|----------|---------|-------------|
| `name` | string | yes | — | Unique tool name (must not collide with built-in tools) |
| `description` | string | yes | — | What the tool does (shown to the AI) |
| `version` | string | no | `""` | Version string |
| `runtime` | object | yes | — | How to run the script |
| `protocol` | string | no | `simple` | `simple` or `json_rpc` |
| `parameters` | JSON Schema | yes | — | Parameters the AI should pass |
| `permissions` | list | no | `[]` | Capabilities granted to the script (see below) |

### `runtime`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `command` | string | — | Executable to run (e.g. `python3`, `node`, `bash`) |
| `args` | list | `[]` | Command-line arguments |
| `working_dir` | string | `.` | Working directory relative to manifest |
| `timeout_secs` | integer | `120` | Maximum execution time |
| `env` | map | `{}` | Environment variables |

---

## Simple Protocol

The default protocol. Gasket spawns the process, writes the arguments as a single JSON line to **stdin**, waits for the process to exit, and reads the result as JSON from **stdout**.

```
Gasket  → stdin:  {"city":"Tokyo"}\n
Script  → stdout: {"temperature":18,"condition":"cloudy"}\n
Script exits with code 0
```

Requirements:
- Output must be exactly one line of valid JSON on stdout
- Exit code must be 0
- stderr is captured and returned as debug info

---

## JSON-RPC Protocol

Use `protocol: json_rpc` when the script needs to talk back to Gasket during execution — for example, to call the LLM, search memory, or spawn a subagent.

Communication is **JSON-RPC 2.0 over newline-delimited JSON** on stdin/stdout.

### Lifecycle

1. Gasket spawns the process
2. Gasket sends an `initialize` request (`id: 0`) with the tool arguments
3. The script may send **requests** to Gasket (e.g. `llm/chat`) and read **responses**
4. When the script is done, it replies to `initialize` with the final result
5. Gasket reads the result and the process exits

```json
// Gasket → Script (initialize)
{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"city":"Tokyo"}}

// Script → Gasket (ask for LLM help)
{"jsonrpc":"2.0","id":1,"method":"llm/chat","params":{"messages":[{"role":"user","content":"Summarize weather for Tokyo"}]}}

// Gasket → Script (LLM response)
{"jsonrpc":"2.0","id":1,"result":{"content":"It will be cloudy..."}}

// Script → Gasket (final result)
{"jsonrpc":"2.0","id":0,"result":{"summary":"It will be cloudy with a high of 18°C."}}
```

> **Important:** `id: 0` is reserved for `initialize`. Your script should use `id >= 1` for its own requests.

### Daemon Mode (Persistent Process)

JSON-RPC plugins use a persistent daemon under the hood. The process stays alive across multiple tool invocations to avoid cold-start overhead. If it sits idle longer than `timeout_secs`, it is restarted on the next call.

---

## Permissions

JSON-RPC plugins must explicitly declare which engine capabilities they need. The default policy is **deny-all**.

| Permission | RPC Method | What it does |
|------------|------------|--------------|
| `llm_chat` | `llm/chat` | Call the LLM provider (`chat` endpoint) |
| `subagent_spawn` | `subagent/spawn` | Spawn a subagent to handle a task |
| `message_send` | `message/send` | Send messages to channels |
| `user_ask` | `user/ask` | Ask the user questions |

Example manifest:

```yaml
permissions:
  - llm_chat
  - subagent_spawn
```

If a script calls a method it doesn't have permission for, Gasket returns:

```json
{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"Permission denied: llm/chat"}}
```

### `llm/chat` Parameters

```json
{
  "messages": [
    {"role": "system", "content": "You are a helpful assistant"},
    {"role": "user", "content": "Hello"}
  ],
  "model": "optional-model-id"
}
```

Returns:

```json
{
  "content": "Hello! How can I help?",
  "usage": {"input_tokens": 10, "output_tokens": 5, "total_tokens": 15}
}
```

### `memory/search` Parameters

```json
{
  "query": "project deadlines",
  "limit": 5
}
```

### `memory/write` Parameters

```json
{
  "content": "User prefers dark mode",
  "category": "preference"
}
```

### `subagent/spawn` Parameters

```json
{
  "task": "Refactor this Python function",
  "model_id": "optional-model-id"
}
```

Returns:

```json
{
  "id": "sub-123",
  "response": {"content": "Here is the refactored code..."}
}
```

---

## Error Codes

Standard JSON-RPC codes are used:

| Code | Meaning |
|------|---------|
| `-32700` | Parse error |
| `-32600` | Invalid request |
| `-32601` | Method not found |
| `-32602` | Invalid params |
| `-32603` | Internal error |
| `-32000` | Permission denied |

---

## Python JSON-RPC Example

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
        # Call LLM for help
        send({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "llm/chat",
            "params": {
                "messages": [{"role": "user", "content": f"Weather summary for {city}"}]
            }
        })
        llm_resp = read_line()
        summary = llm_resp.get("result", {}).get("content", "No response")
        send({"jsonrpc": "2.0", "id": req_id, "result": {"summary": summary}})
    else:
        send({"jsonrpc": "2.0", "id": req_id, "error": {"code": -32601, "message": f"Unknown method: {method}"}})
```

---

## Security Notes

- Plugins run as child processes with the same OS privileges as Gasket
- There is no sandbox by default (use your OS sandboxing if needed)
- JSON-RPC callbacks are strictly gated by the permission list
- stdout lines that are not valid JSON-RPC are logged and discarded
- Single messages are capped at 1 MiB to prevent memory exhaustion

---

## Related Documents

- [Module Details](modules-en.md) — Internal module design
- [Tool System](tools-en.md) — How Gasket's native tools work
- [Hooks System](hooks-en.md) — Another way to extend Gasket
