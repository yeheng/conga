# Cron Module

> AI's Alarm Clock + To-Do List

---

## One-Sentence Understanding

**Cron is AI's alarm clock + to-do list.** When the time comes, AI automatically executes preset tasks.

```mermaid
flowchart LR
    A[Set time] --> B[Time's up] --> C[AI executes automatically] --> D[Send result]
```

---

## Real-Life Examples

| Scenario | Similar to Cron... |
|----------|--------------------|
| Phone alarm rings at 7am daily | Send good morning message daily at 7am |
| Calendar reminder for Monday meeting | Send weekly report reminder every Monday |
| Timer to turn off stove in 30 min | Remind to check tasks after 30 minutes |
| Birthday reminder sends wishes yearly | Send wishes automatically every year |

---

## What Cron Can Do

```mermaid
mindmap
  root((Cron Scheduled Tasks))
    Information Retrieval
      Daily weather query
      Weekly news fetch
      Periodic website status check
    Report Generation
      Daily data summary
      Weekly report auto-generation
      System health report
    Maintenance Tasks
      Auto cleanup old files
      Database backup
      Log archiving
    Reminder Notifications
      Meeting reminders
      Deadline reminders
      Anniversary wishes
```

---

## Composition of Scheduled Tasks

Each scheduled task contains:

```mermaid
flowchart TB
    subgraph A Scheduled Task
        A[When to execute<br/>Cron expression]
        B[What to do<br/>Task content]
        C[Send to whom<br/>Target channel]
        D[Enabled status<br/>On/Off]
    end

    A --> E[Task execution]
    B --> E
    C --> E
    D --> E
```

### 1. When to Execute? (Cron Expression)

The Cron expression is a time format that tells the system **when** to execute the task:

```mermaid
flowchart LR
    subgraph Time Format
        S[Seconds] --> M[Minute]
        M --> H[Hour]
        H --> D[Day]
        D --> Mo[Month]
        Mo --> W[Weekday]
    end
```

The implementation supports **6-field** (`sec min hour day month weekday`) or **7-field** (with year) Cron expressions. 5-field input is auto-normalized by prepending `0` for seconds. The `cron` tool explicitly documents the 7-field format.

| Expression | Meaning | Example |
|------------|---------|---------|
| `0 0 9 * * *` | Every day at 9:00 | Daily morning report at 9am |
| `0 0 */6 * * *` | Every 6 hours | Check email every 6 hours |
| `0 0 9 * * 1` | Every Monday at 9:00 | Weekly report reminder every Monday |
| `0 0 0 1 * *` | 1st of every month | Monthly report on the 1st |
| `0 */5 * * * *` | Every 5 minutes | Check system status every 5 minutes |

### 2. What to Do? (Task Content)

Task content tells AI what operation to perform:

```mermaid
flowchart TB
    subgraph Task Types
        A1[Let AI think and process<br/>Send prompt to AI]
        A2[Execute tool directly<br/>e.g., send email]
    end

    A1 --> B1[Example: Query today's weather<br/>Compile into report]
    A2 --> B2[Example: Execute backup script]
```

### 3. Send to Whom? (Target Channel)

Task execution results can be sent to:

```mermaid
flowchart LR
    R[Task result] --> T[Telegram]
    R --> D[Discord]
    R --> S[Slack]
    R --> W[Webhook]
    R --> L[Local log]
```

---

## System Architecture

### File Storage Design

Cron jobs are defined as Markdown files with YAML frontmatter in `~/.gasket/cron/*.md`:

```mermaid
flowchart TB
    subgraph Config Files
        F1[morning-weather.md]
        F2[daily-report.md]
        F3[weekly-backup.md]
    end

    subgraph Each File Contains
        C1[Time setting<br/>cron: 0 0 9 * * *]
        C2[Task content<br/>Query weather and report]
        C3[Target setting<br/>channel: telegram]
    end

    F1 --> C1
    F2 --> C2
    F3 --> C3
```

### Execution Flow

```mermaid
sequenceDiagram
    participant Clock as System Clock
    participant Cron as Cron Service
    participant File as Task Files
    participant DB as State Storage
    participant AI as AI Brain
    participant User as User

    Note over Cron: Check once per minute

    loop Every minute
        Cron->>File: Read all task configs
        Cron->>DB: Query last execution time
        Cron->>Cron: Calculate next execution time

        alt Time to execute
            Cron->>AI: Trigger task execution
            AI->>AI: Process task content
            AI-->>User: Send result
            Cron->>DB: Update execution state
        else Not yet time
            Cron->>Cron: Continue waiting
        end
    end
```

---

## Hybrid Architecture Design

Cron uses a **file + database** hybrid design:

```mermaid
flowchart TB
    subgraph Config Layer (Files)
        F[Task definition files<br/>.md format]
        F1[Human-editable]
        F2[Version control friendly]
        F3[Hot reload support]
    end

    subgraph State Layer (Database)
        D[SQLite database]
        D1[Last execution time]
        D2[Next execution time]
        D3[Execution count stats]
    end

    subgraph Memory Layer (Runtime)
        M[Task scheduler]
        M1[Cache task list]
        M2[Calculate execution time]
        M3[Trigger execution]
    end

    F --> M
    D --> M
    M --> D
```

**Why this design?**
- **Files store config**: You can edit files directly, manage with Git, clear at a glance
- **Database stores state**: Records last execution time, won't lose on restart, can detect missed tasks

### Database Schema

```sql
CREATE TABLE cron_state (
    job_id TEXT PRIMARY KEY,
    last_run TIMESTAMP,
    next_run TIMESTAMP
);
```

---

## Actual Usage Scenarios

### Scenario 1: Daily Weather Report

```mermaid
sequenceDiagram
    participant Time as Every day 9:00
    participant Cron as Cron Service
    participant AI as AI Brain
    participant API as Weather API
    participant User as User's Phone

    Time->>Cron: Trigger task
    Cron->>AI: Execute: query Guangzhou weather
    AI->>API: Fetch weather data
    API-->>AI: Return weather info
    AI->>AI: Format into friendly message
    AI-->>User: Send: Today Guangzhou is sunny, 25°C...
```

**Task file example:**
```markdown
---
name: Daily Weather
cron: "0 0 9 * * *"
channel: telegram
to: "User ID"
---

Query Guangzhou today and next three days' weather,
send to user in a friendly tone.
```

### Scenario 2: System Auto-Maintenance

```mermaid
flowchart TB
    subgraph System Maintenance Tasks
        T3[Every hour<br/>Check cron config updates]
    end

    T3 --> C[Cron Service]
```

These tasks **execute tools directly**, bypassing AI, zero cost:
- `system-cron-refresh`: Reload task configuration

### Scenario 3: Missed Task Catch-up

```mermaid
sequenceDiagram
    participant System as System Startup
    participant Cron as Cron Service
    participant Task as Daily Backup Task
    participant User as User

    Note over System: System was down for 8 hours last night

    System->>Cron: Start service
    Cron->>Task: Check last execution time
    Task-->>Cron: Executed yesterday at 9:00
    Cron->>Cron: Next should be today 9:00
    Cron->>Cron: Now 10:00, already missed!
    Cron->>Task: Execute catch-up immediately
    Task->>User: Send backup completion notification
```

---

## Task Lifecycle

```mermaid
stateDiagram-v2
    [*] --> Created: Add task file
    Created --> Enabled: Enable task
    Enabled --> Waiting: Calculate next execution time
    Waiting --> Executing: Time reached
    Executing --> Completed: Execution successful
    Completed --> Waiting: Calculate next execution time
    Executing --> Failed: Execution failed
    Failed --> Waiting: Log error, wait for next time
    Enabled --> Disabled: Manual disable
    Disabled --> Enabled: Manual enable
    Disabled --> [*]: Delete file
```

---

## How to Use

### 1. View All Tasks

```bash
gasket cron list
```

Example output:
```
Daily Weather
  Time: Every day 9:00
  Status: Enabled ✓
  Next: Tomorrow 9:00

Weekly Report
  Time: Every Monday 9:00
  Status: Enabled ✓
  Next: Next Monday 9:00
```

### 2. Add New Task

```bash
# CLI method
gasket cron add "Daily Weather" "0 0 9 * * *" "Query Guangzhou weather and send"

# Or create file ~/.gasket/cron/daily-weather.md
```

### 3. Enable/Disable Task

```bash
gasket cron enable daily-weather   # Enable
gasket cron disable daily-weather  # Disable
```

### 4. Show Task Details

```bash
gasket cron show daily-weather
```

### 5. Remove Task

```bash
gasket cron remove daily-weather
```

### 6. Refresh Tasks from Disk

```bash
gasket cron refresh
```

### 7. Manually Edit Task File

Edit files directly, the system auto-detects changes:

```bash
vim ~/.gasket/cron/daily-weather.md
# Save after editing, takes effect immediately, no restart needed
```

---

## FAQ

**Q: What if the computer was shut down, what about missed tasks?**
A: The system remembers the next execution time. After booting, it checks for missed tasks and executes them immediately.

**Q: Do I need to restart after modifying task files?**
A: No! The system monitors file changes, changes take effect immediately after saving.

**Q: How many tasks can I set?**
A: No limit, but plan reasonably to avoid too many tasks executing at the same time.

**Q: Will failed tasks retry?**
A: Each task executes independently. On failure, it logs and waits for the next execution time to retry.
