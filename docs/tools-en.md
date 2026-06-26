# Tools System

> The "Hands and Feet" of AI

---

## One-Sentence Understanding

**Tools are AI's hands and feet** - they allow AI to interact with the outside world beyond just talking.

> Analogy: Like a person with hands can write, cook, and build; AI with tools can search, code, and execute commands.

---

## Why Do We Need Tools?

```mermaid
flowchart TB
    subgraph NoTools["❌ AI Without Tools"]
        Q1["User: What's the weather?"]
        A1["AI: Sorry, I don't know..."]
        
        Q2["User: Read this file"]
        A2["AI: I can't access files..."]
    end
    
    subgraph WithTools["✅ AI With Tools"]
        Q3["User: What's the weather?"]
        T1["Search weather API"] --> A3["AI: It's sunny, 25°C"]
        
        Q4["User: Read this file"]
        T2["Read file tool"] --> A4["AI: File content is..."]
    end
```

Without tools, AI is just a "brain in a jar" - smart but powerless.

---

## What Can Tools Do?

```mermaid
mindmap
  root((Tools))
    Filesystem
      ::icon(📁)
      Read files
      Write files
      List directories
    Web
      ::icon(🌐)
      Search web
      Fetch URLs
    System
      ::icon(⚡)
      Run commands
      Spawn subagents
    Communication
      ::icon(💬)
      Send messages
    History
      ::icon(🧠)
      Query history
      Search history
    Schedule
      ::icon(⏰)
      Create cron jobs
```

---

## Tool Categories

### 1. Filesystem Tools

```mermaid
sequenceDiagram
    participant User
    participant AI
    participant Tool as read_file Tool
    participant FS as File System
    
    User->>AI: Read main.rs
    AI->>Tool: Call read_file
    Tool->>FS: Read file
    FS-->>Tool: Return content
    Tool-->>AI: File content
    AI-->>User: Explain the code
```

| Tool | Purpose | Example |
|------|---------|---------|
| `read_file` | Read file content | "Read config.yaml" |
| `write_file` | Create new file | "Create hello.py" |
| `edit_file` | Modify existing file | "Add function to main.rs" |
| `list_dir` | List directory | "Show files in src/" |

### 2. Web Tools

```mermaid
flowchart LR
    User["User: Search Rust tutorial"] --> AI
    AI --> Search["web_search Tool"]
    Search --> API["Search API<br/>(Brave/Tavily)"]
    API --> Results["Search Results"]
    Results --> AI --> Answer["Curated Answer"]
```

| Tool | Purpose | Example |
|------|---------|---------|
| `web_search` | Search the web | "Find latest Rust version" |
| `web_fetch` | Fetch specific URL | "Read this article" |

### 3. System Tools

```mermaid
flowchart TB
    subgraph Safe["✅ Safe Commands"]
        S1["git status"]
        S2["cargo build"]
        S3["ls -la"]
    end
    
    subgraph Dangerous["❌ Blocked Commands"]
        D1["rm -rf /"]
        D2["curl evil.com"]
    end
    
    subgraph Policy["Command Policy"]
        P1["allow_list<br/>Only permitted"]
        P2["deny_list<br/>Block dangerous"]
        P3["allow_all<br/>Everything (risky!)"]
    end
```

| Tool | Purpose | Safety |
|------|---------|--------|
| `exec` | Run shell commands | Configurable policy |
| `spawn` | Create subagent | Isolated execution, supports model selection |
| `spawn_parallel` | Create multiple subagents | Max 10 tasks, configurable concurrency |
| `new_session` | Start fresh session | Clears history, new session key |
| `clear_session` | Clear current session | Keeps session key |
| `message` | Send message to user | For cron/background tasks |

### 4. Communication Tools

```mermaid
sequenceDiagram
    participant Main as Main Agent
    participant Tool as send_message Tool
    participant User
    
    Main->>Tool: Send "Task done!"
    Tool->>User: Telegram message
    User-->>Tool: Reply
    Tool-->>Main: Response
```

| Tool | Purpose | Example |
|------|---------|---------|
| `send_message` (`MessageTool`) | Send to channel | "Notify user on Telegram" |

### 5. History Query Tools

```mermaid
flowchart LR
    Query["User query"] --> Search["history_query"]
    Search --> DB[(SQLite)]
    DB --> Results["Matching messages"]
    Results --> AI["AI with context"]
```

| Tool | Purpose | Example |
|------|---------|---------|
| `history_query` | Query conversation history by keywords | "What did I say yesterday?" |
| `history_search` | Semantic search through history (requires `embedding` feature) | "Find discussions about DB design" |

### 6. Schedule Tools

| Tool | Purpose | Example |
|------|---------|---------|
| `cron` | Create scheduled task | "Remind me daily at 9am" |
| `script` (`PluginTool`) | External script with YAML manifest | Custom business logic |

---

## How AI Uses Tools

```mermaid
sequenceDiagram
    participant User
    participant AI
    participant Kernel
    participant Tool
    
    User->>AI: "What's the weather in Beijing?"
    
    AI->>Kernel: Need to use tool?
    Kernel->>AI: Yes, call web_search
    
    AI->>Tool: web_search("Beijing weather")
    Tool->>Tool: Call weather API
    Tool-->>AI: {"temp": 25, "condition": "Sunny"}
    
    AI->>Kernel: Now I have info
    Kernel-->>AI: Generate response
    AI-->>User: "It's sunny and 25°C in Beijing"
```

### Decision Flow

```mermaid
flowchart TB
    Input["User Input"] --> Think["AI Thinking"]

    Think --> Decision{"Need tool?"}

    Decision -->|Yes| Which{"Which tool?"}
    Decision -->|No| Direct["Direct Answer"]

    Which -->|File| FileTool["Read/Write File"]
    Which -->|Info| WebTool["Web Search"]
    Which -->|Command| ExecTool["Execute Command"]
    Which -->|History| HistTool["Query History"]
    Which -->|Restart| SessTool["New Session"]

    FileTool --> Result["Tool Result"]
    WebTool --> Result
    ExecTool --> Result
    MemTool --> Result

    Result --> ThinkAgain["Think Again"]
    ThinkAgain --> Decision

    Direct --> Output["Final Response"]
```

---

## Tool Execution

### Parallel Execution

When AI needs multiple tools, they run in parallel:

```mermaid
flowchart TB
    AI["AI Decision"] -->|"Need 3 files"| Exec["ToolExecutor"]
    
    subgraph Parallel["Parallel Execution"]
        direction LR
        T1["read_file A"]
        T2["read_file B"]
        T3["read_file C"]
    end
    
    Exec --> T1
    Exec --> T2
    Exec --> T3
    
    T1 --> Results["Combined Results"]
    T2 --> Results
    T3 --> Results
    
    Results --> AI
```

Example: "Compare file1.rs, file2.rs, and file3.rs"
- All three files are read simultaneously
- Results combined and sent back to AI

### Tool Context

Tools receive context about the current session:

```rust
struct ToolContext {
    session_key: SessionKey,     // Who is asking
    outbound_tx: Sender<OutboundMessage>, // Real-time message channel
    spawner: Arc<dyn SubagentSpawner>,    // Subagent spawner
    ws_summary_limit: usize,     // Subagent summary length limit (WebSocket)
    token_tracker: Arc<TokenTracker>,     // Token budget tracker
}
```

This allows tools to:
- Know which user/session
- Access allowed directories
- Respect configuration limits

---

## Tool Registry

All available tools are registered in a central registry:

```mermaid
flowchart TB
    subgraph Registry["ToolRegistry"]
        T1["read_file"]
        T2["write_file"]
        T3["web_search"]
        T4["exec"]
        T5["spawn"]
        T6["history_query"]
        T7["history_search"]
        T13["new_session"]
        T14["clear_session"]
        T15["message"]
        T16["script"]
        TN["...more"]
    end

    Kernel -->|Query| Registry
    Registry -->|Return list| Kernel
    Kernel -->|Select| Selected["Appropriate tools"]
```

### Tool Execution Signature

All tools implement the `Tool` trait. The `ctx` parameter is **required**:

```rust
async fn execute(&self, args: Value, ctx: &ToolContext) -> ToolResult;
```

### Tool Definition Format

Each tool defines:

```json
{
  "name": "read_file",
  "description": "Read content of a file",
  "parameters": {
    "type": "object",
    "properties": {
      "path": {
        "type": "string",
        "description": "Path to the file"
      }
    },
    "required": ["path"]
  }
}
```

This is the **JSON Schema** that tells AI how to use the tool.

---

## Tool Approval System

Gasket includes a **tool execution approval** mechanism to prevent AI from executing dangerous operations without confirmation.

### Approval Flow

```mermaid
sequenceDiagram
    participant AI as AI Brain
    participant R as ToolRegistry
    participant CB as ApprovalCallback
    participant U as User/Frontend

    AI->>R: Execute write_file
    R->>R: requires_approval = true
    R->>CB: request_approval
    CB->>U: Show confirmation dialog
    U-->>CB: Approve / Deny / Remember
    CB-->>R: approved?
    alt Approved
        R->>R: Continue execution
    else Denied
        R-->>AI: PermissionDenied
    end
```

### Tools Requiring Approval

The following tools require user confirmation by default:

| Tool | Category | Description |
|------|----------|-------------|
| `write_file` | Filesystem | Create or overwrite files |
| `edit_file` | Filesystem | Modify existing files |
| `exec` | System | Execute shell commands |
| `new_session` | Session | Clear history and create new session |
| `clear_session` | Session | Clear current session history |

**Remember Decision**: In WebSocket frontend, users can check "Remember this decision" to auto-approve/deny future calls of the same tool in the same session.

### No-Approval Tools

The following read-only tools execute without confirmation:

- `read_file`, `list_dir`, `web_search`, `web_fetch`
- `history_query`
- `spawn`, `spawn_parallel`

---

## Safety Design

### Command Policy

```yaml
tools:
  exec:
    policy:
      allowlist: ["git", "cargo", "ls", "cat"]
      denylist: ["rm", "sudo"]
```

| Policy | Description | Risk Level |
|--------|-------------|------------|
| `allowlist` | Only allow specific commands | 🟢 Low |
| `denylist` | Block dangerous commands | 🟡 Medium |
| `allow_all` | Allow everything | 🔴 High |

### Path Restrictions

Enable `restrict_to_workspace` to limit file operations to the workspace directory:

```yaml
tools:
  restrict_to_workspace: true
```

---

## Plugin System: External Tools

Gasket uses an external plugin system to connect custom tools. Plugins are loaded from `~/.gasket/plugins/` via YAML manifests and exposed as native tools.

Example plugin tools:
- Database query tools
- GitHub integration
- Custom business tools

See [Plugin System](plugin-en.md) for details.

---

## Related Modules

- **Kernel**: Decides when to use tools
- **Sandbox**: Isolates tool execution
- **Session**: Provides tool context
