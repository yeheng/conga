# 语言边界决策：Rust 保留 harness 与 host，TS 只经接缝进入

> 日期:2026-09-13 · 状态:已评审通过(架构裁决,任务随本记录落地)
>
> 本文回答一个问题:**「把架构改为 Rust 作 harness 基座、TypeScript 作 host(或 host 上一层)是否合理?」**
> 结论先行:不合理;TS 的正确位置是接缝之外的消费者与扩展,不是 host 本体。

---

## 1. 事实基础

| 层 | 位置 | 规模 | 说明 |
|---|---|---|---|
| harness(内核) | `conga/conga/src/` | ~8.7k 行 | 无状态 `agent_loop`、消息/事件类型、providers、崩溃安全 events.jsonl 存储、`ExtensionApi` 接缝 |
| host | `conga/conga-host/src/` | ~18.7k 行 | `Host::run_turn` 驱动器、会话、四种权限模式、hook 链、token 压缩、MCP 客户端(1.7k)、子代理(1k) |
| wire 契约 | `conga-host/src/wire.rs` + `event_map.rs` | ~0.5k | 语言中立的 JSON 事件协议,`conga exec --json` 与 gateway/Tauri 共用同一 schema |
| 前端 | `web/` | ~2.3k 行 TS | Vue3 + Tauri,只讲 wire 协议 |

关键事实:**真正的接缝不是语言边界,而是两个 JSON 契约**——磁盘上的 `events.jsonl`(持久契约)与 `OutgoingEvent` wire schema(传输契约)。二者均已语言中立。

## 2. 裁决

### 2.1 「TS 作为 host」——否决

- conga-host 18.7k 行不是胶水,是产品正确性的本体:persist 顺序(Assistant 先于工具执行落盘)、torn-tail 自愈、`derive_messages` 纯投影、协作式取消、steer 队列。重写为 TS = 在 GC 语言里重新挣回每一条不变量,用户不可见任何收益。
- Node 成为硬运行时依赖:自托管镜像膨胀、多一个进程、类型定义双份维护。单二进制(`opt-level=z + strip`)是本项目的分发优势。
- 参照系:pi 的扩展生态是 TS 的,因为 pi **整个**是 TS(单一语言,零边界)。抄"上面一层 TS"却把底座留在 Rust,得到两门语言的维护成本加一条 IPC 边界,两边的生态红利都拿不到。

### 2.2 「TS 作为 host 上一层」——有条件接受,落地为 SDK

- `conga exec --json` 的 NDJSON 与 gateway WS 已是同一 schema(`event_map::event_to_ws` → `OutgoingEvent`),TS 上一层**接缝现成**。
- 诚实定性:这不是架构改动,是写一个 SDK。触发前提是有具体 Node 消费者;本次执行视为消费者确认(产品负责人拍板)。

### 2.3 「扩展生态要 TS」——在 ExtensionApi 接缝后解决

- 根问题是「扩展作者不愿意为一个小工具编译 Rust crate」,不是「host 用什么写」。
- 方案:`conga-host` 新增 feature `ext-js`,嵌入 rquickjs(QuickJS)沙箱,脚本经 JSON 子集注册工具/hook。改的是"扩展怎么注册",不是"host 用什么写"。现有 Rust 扩展 API 原样保留,零破坏。

## 3. 不可破坏的不变量(任何语言边界改动都必须守住)

1. `events.jsonl` append-only、崩溃安全(torn-tail 自愈、未知变体 fail-closed)。
2. persist 顺序:Assistant(含 usage)先于其中任何工具执行落盘。
3. history 每轮从日志现派生(`derive_messages`),无旁路内存 transcript。
4. wire schema「只加字段,不改名」(wire.rs 头注即契约)。
5. 权限系统是工具执行的唯一通道——任何扩展机制(含 JS 脚本)不得绕过 `before_tool_call` 审批。

## 4. 重新评估的触发条件(满足其一才重提 host 语言归属)

1. 团队 Rust 产能枯竭:host 迭代速度成为瓶颈的量化证据(如连续两个迭代因 Rust 产能砍功能)。
2. 产品形态变为 npm 分发的库(conga 的主要交付物是 Node 依赖)。

在此之前,本决策为终局裁决,不再讨论。

## 5. 随本裁决落地的任务

| 任务 | 内容 | 状态 |
|---|---|---|
| 1 | `sdk-ts/`:npm 包 `conga`,包装 `conga exec --json`,NDJSON → 类型化事件;wire fixture 与 Rust 侧共享并有漂移测试 | 本次执行 |
| 2 | `conga-host` feature `ext-js`:rquickjs 沙箱扩展脚本(`--ext-js <path>`),JSON 子集注册工具/hook | 本次执行 |
| 3 | 本决策记录 | 已完成 |
