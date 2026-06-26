# System Architecture

> Gasket overall architecture design — how the components work together

---

## One-Sentence Summary

Gasket is like an **operating system for an AI assistant**, connecting user input, AI brain, memory, and tools together.

---

## Overall Architecture Diagram

```mermaid
flowchart TB
    subgraph User_Layer["👤 User Layer"]
        CLI[Command Line]
        TG[Telegram]
        DC[Discord]
        SL[Slack]
        FS[Feishu]
        WS[WebSocket]
    end
    
    subgraph Access_Layer["📡 Access Layer"]
        Router[Message Router]
    end
    
    subgraph Core_Layer["⚙️ Core Layer"]
        Session[Session Manager]
        Kernel[AI Brain]
        Hook[Hook System]
    end
    
    subgraph Capability_Layer["🧰 Capability Layer"]
        Tools[Toolbox]
        Memory[Memory Bank]
        Skills[Skills Library]
    end
    
    subgraph External_Services["🌐 External Services"]
        LLM[LLM API<br/>GPT/Claude/DeepSeek]
        Web[The Internet]
        File[Local Files]
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
```

---

## Layer Responsibilities

### 1. User Layer: Multiple Entry Points

```mermaid
flowchart LR
    subgraph Where_Users_Come_In["Where users come in"]
        A[Command Line]
        B[Telegram]
        C[Discord]
        D[Others]
    end
    
    subgraph Unified_Processing["Unified processing"]
        E[Unified Message Format]
    end
    
    A --> E
    B --> E
    C --> E
    D --> E
```

No matter which channel the user comes from, messages are converted into a unified format:

- User ID
- Message content
- Channel type
- Timestamp

### 2. Access Layer: Message Routing

```mermaid
flowchart TB
    subgraph Message_Routing["Message Routing"]
        R[Router]
        
        R -->|User A| S1[Session A]
        R -->|User B| S2[Session B]
        R -->|User C| S3[Session C]
    end
    
    S1 --> Out[Outbound]
    S2 --> Out
    S3 --> Out
    
    Out --> TG[Reply Telegram]
    Out --> DC[Reply Discord]
```

**Key Design**:

- Each user has an independent Session
- Sessions do not interfere with each other
- Sessions are automatically created and cleaned up

### 3. Core Layer: The Three Musketeers

```mermaid
flowchart TB
    subgraph Core_Trio["Core Trio"]
        Session[Session Manager<br/>The Butler]
        Kernel[AI Brain<br/>The Thinker]
        Hook[Hook System<br/>The Checkpoint]
    end
    
    User[User] --> Session
    Session --> Hook
    Hook --> Kernel
    Kernel --> Session
    Session --> User
    
    style Session fill:#E3F2FD
    style Kernel fill:#FFF3E0
    style Hook fill:#F3E5F5
```

| Component | Analogy | Responsibility |
|-----------|---------|----------------|
| Session | Butler | Receives guests, prepares materials, takes notes |
| Kernel | Brain | Thinks, decides, generates replies |
| Hook | Checkpoint | Security checks, data injection, logging |

### 4. Capability Layer: Toolbox

```mermaid
mindmap
  root((Capability Layer))
    Toolbox
      File Operations
      Web Search
      Command Execution
      Subagents
    Memory Bank
      Short-term History
      Long-term Memory
      User Profile
    Skills Library
      Code Review
      Writing Assistant
      Data Analysis
```

---

## Data Flow

### Complete Request Processing Flow

```mermaid
sequenceDiagram
    participant U as User
    participant R as Router
    participant S as Session
    participant H as Hooks
    participant K as Kernel
    participant L as LLM
    participant T as Tools
    participant M as Memory
    
    U->>R: Send message
    R->>R: Route to corresponding Session
    
    activate S
    S->>M: Load user memory
    M-->>S: Return memory
    
    S->>H: BeforeRequest hook
    H-->>S: Continue / Abort
    
    S->>S: Save user message to history
    S->>S: Assemble context
    
    S->>K: Request processing
    
    activate K
    K->>L: Send prompt
    L-->>K: Return thinking + possible tool calls
    
    alt Needs tool
        K->>T: Call tool
        T-->>K: Return result
        K->>L: Request again with result
        L-->>K: Final reply
    end
    
    K-->>S: Return result
    deactivate K
    
    S->>H: AfterResponse hook
    S->>S: Save AI reply
    S->>M: Update access records
    
    S-->>R: Return result
    deactivate S
    
    R-->>U: Display reply
```

---

## Module Deep Dive

### Session: Session Management

```mermaid
flowchart TB
    subgraph Inside_Session["Inside Session"]
        A[Receive Request]
        B[Load Context]
        C[Call Kernel]
        D[Save Result]
    end
    
    subgraph Context_Composition["Context Composition"]
        S[System Prompt]
        SK[Skills]
        H[History]
        M[Memory]
        Q[Current Question]
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
```

### Kernel: AI Brain

```mermaid
flowchart TB
    subgraph Kernel_Thinking_Loop["Kernel Thinking Loop"]
        Start([Start]) --> Input[Receive Context]
        Input --> Ask[Ask LLM]
        Ask --> Analyze{Analyze Reply}
        
        Analyze -->|Needs tool| Tool[Execute Tool]
        Tool --> Result[Tool Result]
        Result --> Ask
        
        Analyze -->|Direct answer| Output[Output Result]
        Analyze -->|Limit reached| Output
        
        Output --> End([End])
    end
    
    style Tool fill:#FFD700
```

### Tools: Tool System

```mermaid
flowchart TB
    subgraph Tool_Registry["Tool Registry"]
        R[ToolRegistry]
    end
    
    subgraph Tool_Categories["Tool Categories"]
        F[File Tools]
        W[Web Tools]
        E[Execution Tools]
        S[Subagents]
        M[Memory Tools]
    end
    
    R --> F
    R --> W
    R --> E
    R --> S
    R --> M
    
    F --> FS[Local Files]
    W --> Web[The Internet]
    E --> Shell[Shell Commands]
    S --> Sub[Create Sub-AI]
    M --> Mem[Read/Write Memory]
```

---

## Key Design Decisions

### 1. Pure Function Kernel

```mermaid
flowchart LR
    subgraph Input
        A[Context
        Config
        Tools]
    end
    
    subgraph Kernel_Box
        B[Black Box Processing
        No Side Effects
        Predictable]
    end
    
    subgraph Output
        C[Reply Content
        Tool Calls]
    end
    
    A --> B --> C
    
    style B fill:#C8E6C9
```

**Benefits**:

- Same input, same output
- Easy to test
- Convenient for retry and caching

### 2. ContextBuilder Pattern

```mermaid
flowchart TB
    subgraph Old_Way
        O[Option&lt;Context&gt;]
        O -->|Some| P[Persistent]
        O -->|None| S[Stateless]
    end

    subgraph New_Way
        CB[ContextBuilder]
        CB -->|Assemble| CTX[Complete Context]
    end

    style CB fill:#C8E6C9
```

**Benefits**:

- Unified interface for history, memory, and summaries
- Supports three-phase token budget (bootstrap/scenario/on_demand)
- Cleaner code

### 3. File + Database Hybrid Storage

```mermaid
flowchart TB
    subgraph Cron_Jobs
        F[Markdown Files
Human Readable]
        D[SQLite State
Machine Efficient]
    end
    
    subgraph Memory
        F2[Markdown Files
Human Editable]
        D2[SQLite Index
Fast Query]
    end
    
    F <-->|Config| D
    F2 <-->|Content| D2
```

**Benefits**:

- Human editable (Markdown)
- Machine high performance (SQLite)
- Version control friendly

---

## Extension Points

### 1. Hooks: Custom Behavior

```mermaid
flowchart LR
    A[BeforeRequest] --> B[Processing]
    B --> C[AfterResponse]
    
    A --> A1[Profanity Filter]
    A --> A2[Input Formatting]
    
    C --> C1[Logging]
    C --> C2[Send Notification]
```

### 2. Skills: Custom Capabilities

```mermaid
flowchart TB
    User[User] --> Core[Core System]
    
    subgraph Skill_Plugins["Skill Plugins"]
        S1[Code Review Skill]
        S2[Writing Assistant Skill]
        S3[Data Analysis Skill]
    end
    
    Core --> S1
    Core --> S2
    Core --> S3
    
    S1 --> Core
    S2 --> Core
    S3 --> Core
```

### 3. Plugin: External Tool Services

```mermaid
flowchart TB
    Gasket[Gasket Core]
    Plugin[Plugin System]
    
    subgraph External_Services["External Scripts"]
        S1[Python Script]
        S2[Node.js Script]
        S3[Custom Tool]
    end
    
    Gasket --> Plugin
    Plugin --> S1
    Plugin --> S2
    Plugin --> S3
```

---

## Deployment Modes

### Mode 1: CLI Interactive Mode

```mermaid
flowchart LR
    User[User] --> CLI[gasket agent]
    CLI --> Engine[Engine Core]
    Engine --> LLM
```

### Mode 2: Gateway Service Mode

```mermaid
flowchart TB
    subgraph External_Users["External Users"]
        T[Telegram User]
        D[Discord User]
    end
    
    subgraph Gasket_Service["Gasket Service"]
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

### Mode 3: Hybrid Mode

```mermaid
flowchart TB
    User[User] --> Choice{Choose?}
    
    Choice -->|Quick task| CLI[gasket agent]
    Choice -->|Long-term service| Gateway[gasket gateway]
    
    CLI --> Engine
    Gateway --> Engine
    
    Engine --> LLM
```

---

## Summary

```mermaid
mindmap
  root((Gasket Architecture))
    User Layer
      Multi-channel Access
      Unified Message Format
    Access Layer
      Router Routing
      Session Management
    Core Layer
      Pure Function Kernel
      Flexible Hook System
    Capability Layer
      Rich Tools
      Long-term Memory
      Dynamic Skills
    Design Philosophy
      Simple and Predictable
      Human Friendly
      Extensible
```
