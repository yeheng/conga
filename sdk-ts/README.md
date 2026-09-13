# conga-sdk

conga agent harness 的 TypeScript SDK。它不做任何自己的协议——只是包装
`conga exec --json`，把 NDJSON wire 流解析成类型化事件。wire schema 与
gateway / Tauri 桌面端完全同源（`conga/conga-host/src/wire.rs`），并由
Rust 侧的 golden-fixture 测试逐字节锁定（见 `test/fixtures/wire-events.ndjson`）。

## 安装与构建

```bash
cd sdk-ts
npm install
npm test        # 构建 + fixture 契约测试 + 伪造二进制的端到端测试
```

## 用法

### 一次性跑完一轮（CI / 脚本）

```ts
import { runExec, isDoneWithSummary } from "conga-sdk";

const result = await runExec({ task: "修复 src/app.ts 里的编译错误", mode: "full-auto" });
if (result.exitCode !== 0) throw new Error("turn failed");

for (const ev of result.events) {
  if (ev.type === "tool_start") console.log(`tool: ${ev.name}`);
  if (isDoneWithSummary(ev)) console.log(`tokens: ${ev.usage_in}/${ev.usage_out}`);
}
```

### 流式消费（逐事件）

```ts
import { exec } from "conga-sdk";

const run = exec({ task: "你好", congaBin: "/path/to/conga" });
for await (const ev of run) {
  if (ev.type === "content") process.stdout.write(ev.content);
}
const { exitCode } = await run.wait();
```

可运行的完整示例见 `examples/quickstart.ts`（≤20 行）。

## 上下文管理

SDK 是无状态消费者：每次 `exec()` 都 spawn 一个新的 `conga exec` 进程，
会话上下文完全活在 harness 侧（追加写 `events.jsonl`，每轮从日志现派生
历史，超限自动压缩）。多轮连续推进用 `resume`：

```ts
import { runExec } from "conga-sdk";

await runExec({ task: "分析这个仓库的模块结构" });                      // 第 1 轮
await runExec({ task: "把结论写成 docs/arch.md", resume: "last" });   // 第 2 轮，带上下文
```

并行多会话时用具体 session id（从 `run.on("stderr", ...)` 的
`[exec] session <id>` 行捕获）。

## 工具与 hook（`extJs`）

工具/hook 的实现位置在 harness 内嵌的 QuickJS 沙箱脚本，而非 Node 侧。
SDK 只负责把脚本传给进程（需 conga 以 `--features ext-js` 构建）：

```ts
await runExec({ task: "查一下 feature/x 的 CI 状态", extJs: ["tools/ci.js"] });
```

```js
// tools/ci.js —— 沙箱内无 IO/模块加载器；工具照常过权限审批
conga.registerTool({
  name: "ci_probe",
  description: "查询分支 CI 状态",
  parameters: { type: "object", properties: { branch: { type: "string" } } },
  risk: "low",
  execute(args) { return { ok: true, branch: args.branch ?? "main" }; },
});
conga.registerBeforeToolCall(({ toolName, args }) => {
  if (toolName === "bash" && String(args.cmd).includes("rm -rf"))
    return { block: "危险命令" };      // 返回 undefined = 放行；{ modify: {...} } = 改参
});
conga.registerAfterToolCall(({ isError }) => {
  if (isError) return { text: "[已脱敏]", isError: true };
});
```

## 退出码契约（`conga exec`）

| 退出码 | 含义 |
|---|---|
| `0` | turn 正常完成 |
| `1` | turn 出错（`events` 末尾会有 `error` 事件） |
| `130` | 被中止（SIGINT / `run.cancel()`） |
| `2` | 用法 / 配置错误（未产出事件流） |

## 事件类型

见 `src/wire.ts`。要点：

- `conga exec --json` 每轮发出：`content` / `thinking` / `tool_start` /
  `tool_end` / `error`，最后一行总是 `done`（成功时带 usage 汇总字段）。
- `busy` / `queued` / `approval_request` / `subagent_*` 是 gateway 独有事件，
  parser 同样识别，便于未来把 SDK 指向 gateway 的 WebSocket。

## 设计约束（来自架构裁决 docs/plans/2026-09-13-language-boundary-decision.md）

- 零运行时依赖，只依赖 Node 内置模块（≥18）。
- 不发明新协议；Rust 侧改 schema 时必须同步重新生成 fixture
  （`CONGA_REGEN_FIXTURE=1 cargo test -p conga-host wire_fixture`），
  两边测试会同时失败并指向同一份文件。
