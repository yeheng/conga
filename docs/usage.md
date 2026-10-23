# conga 使用文档

> 本文教你如何把 conga 跑起来:配置 LLM、运行后端网关与 CLI、启动 Web / 桌面前端、容器化部署。
> 想理解内部架构,请阅读 [架构设计](./architecture.md)。

conga 由两部分组成:

- **后端**(`conga/`,Rust 工作区):一个 WebSocket 网关服务器 `conga-gateway`,外加一个终端 REPL `conga`。
- **前端**(`web/`,Vue 3):Web 应用,同一份代码可打包成 Tauri 桌面应用。前端经 `ws://<host>:3000` 连接后端网关。

---

## 1. 环境要求

| 组件 | 用途 | 版本/说明 |
|---|---|---|
| **Rust** 工具链 | 构建后端 | 稳定版(stable);`cargo`、`rustc` |
| **Node.js** + **pnpm** | 构建/开发前端 | Node ≥ 18;包管理用 **pnpm**(仓库已标准化) |
| **LLM API Key** | 接入大模型 | 任一 OpenAI 兼容服务(DeepSeek/智谱/xAI/Groq/Ollama/vLLM 等)或 Anthropic |
| **Tauri 2 系统依赖**(仅桌面端) | 打包桌面应用 | macOS:Xcode CLT;Windows:MSVC + WebView2;Linux:webkit2gtk 等 |

> 后端 release profile 已做体积优化(`opt-level="z"`、`lto="fat"`、`strip`),最终二进制较小。

---

## 2. 快速开始(5 分钟)

```bash
# 1) 克隆
git clone https://github.com/YeHeng/conga.git
cd conga

# 2) 配置后端 LLM(必填三项)
cp conga/.env.example conga/.env
#   编辑 conga/.env:
#     CONGA_LLM_BASE_URL=https://api.deepseek.com/v1
#     CONGA_LLM_KEY=你的-key
#     CONGA_LLM_MODEL=deepseek-chat
#     CONGA_LLM_API=openai        # 或 anthropic

# 3) 启动后端网关(监听 127.0.0.1:3000)
cd conga && cargo run --release --bin conga-gateway

# 4) 另开终端,启动 Web 前端(浏览器开发模式,端口 1420)
cd web && pnpm install && pnpm dev
#   打开 http://localhost:1420,开始对话
```

> 也可以跳过前端,直接用终端:`cd conga && cargo run --release --bin conga`(见 §4.2)。

---

## 3. 后端配置(`.env` / `config.toml`)

后端配置支持两种形式,优先级从低到高:

1. **配置文件 `config.toml`**(基础层):放在 `./.conga/config.toml`(项目级)或 `~/.conga/config.toml`(全局),模板见 `conga/config.example.toml`。用 `$CONGA_CONFIG` 可显式指定路径(不存在则启动报错)。
2. **环境变量 + dotenvy**(`conga/.env`,模板 `conga/.env.example`):**同名环境变量覆盖配置文件**。

完整优先级链:`config.toml` < `.env` < 进程环境变量 < `~/.conga/settings.json`(Web UI,每次 LLM 调用前重读)。三处都没配才回退内置默认值;`config.toml` 里的键名与环境变量一一对应(分节写法,如 `[llm] base_url` → `CONGA_LLM_BASE_URL`),未知键启动时报错(防拼错),空字符串视为未设置。

`conga` CLI、`conga-gateway`、`conga-rag` 三个入口启动时都会先加载 `config.toml`。

`conga-rag` 另有独立的 `rag.toml`(数据源/分块/向量库,模板见 `conga/rag.example.toml`,查找顺序 `$CONGA_RAG_CONFIG` → `./rag.toml` → `~/.conga/rag.toml`)。`config.toml` 的 `[rag]` 节对应 `CONGA_RAG_*` 环境变量,优先级高于 `rag.toml` 文件内的同名配置。

### 3.1 必填:LLM 连接

| 变量 | 说明 | 示例 |
|---|---|---|
| `CONGA_LLM_BASE_URL` | provider 基础 URL | `https://api.deepseek.com/v1` |
| `CONGA_LLM_KEY` | API key | `sk-...` |
| `CONGA_LLM_MODEL` | 模型 id | `deepseek-chat` |
| `CONGA_LLM_API` | 协议族:`openai`(默认)或 `anthropic` | `openai` |

### 3.1.1 Web 端运行时覆盖(图形界面配置方式)

ChatHeader 的齿轮按钮(**Model Settings**)可以不碰 `.env` 直接配置 LLM:设置持久化到 `~/.conga/settings.json`(原子写),**每次 LLM 调用前重读**——保存后下一条消息即生效,无需重启。

- **优先级**:`settings.json` > 进程 env(`.env`)。浏览器端走 `GET/PUT /api/settings`,桌面端走 Tauri IPC,同一份文件。
- **安全**:API key 从不回传——GET 只返回 `apiKeySet`/`apiKeyHint`(`sk-…ab12` 掩码);PUT 留空 key 表示"保留已存的"。
- **组语义**:取消勾选 Main/Fast = 清除该组(env 配置重新生效);Fast 组控制子代理模型,同样优先于 `CONGA_FAST_LLM_*`。
- **System prompt**:同一对话框下方的 **System prompt** 编辑器(markdown)替换内置基础指令(`CODING_AGENT_PROMPT`);AGENTS.md/CLAUDE.md 项目文档、技能目录、`<environment>` 快照仍然自动附加。Preview 按钮实时预览 markdown 渲染,Reset 清空回到内置。留空 = 内置;上限 64 KB。子代理不受影响(保持内置纪律 prompt)。
- **Max tokens**:同一对话框可设上下文窗口 `maxTokens`(整数 1024–2000000;`null`/留空 = 跟随 `CONGA_CONTEXT_WINDOW`)。优先级 `maxTokens` > `CONGA_CONTEXT_WINDOW` > 128000,保存后下一轮压缩与统计即用新窗口。
- 手动编辑文件也可,格式:`{"llm":{"baseUrl":...,"apiKey":...,"model":...,"api":"openai"},"fastLlm":{...},"systemPrompt":"# 自定义指令\n...","maxTokens":200000}`;损坏文件会被忽略并回退 env(告警在日志)。

### 3.2 Provider 选择

- **OpenAI 兼容(`openai`,默认)**:DeepSeek、智谱 GLM、xAI、Groq、Ollama、vLLM 等任填 base_url + key + model 即可。
- **Anthropic(`anthropic`)**:设 `CONGA_LLM_API=anthropic`,base_url 指向 `https://api.anthropic.com/v1`,用 Claude 模型。

### 3.3 可选:代理

| 变量 | 说明 |
|---|---|
| `CONGA_LLM_PROXY` | http 与 https 通吃的代理(fallback) |
| `CONGA_LLM_HTTP_PROXY` | 仅 http(覆盖上面的 http 部分) |
| `CONGA_LLM_HTTPS_PROXY` | 仅 https(覆盖上面的 https 部分) |

**工具代理(fetch / web_search)**:设置 `CONGA_TOOL_PROXY` 可让 `fetch` 与 `web_search` 工具的出站流量走代理,支持 `http` / `https` / `socks5` / `socks5h`(带认证的代理把 `user:pass` 写进 URL 即可):

| 变量 | 说明 | 示例 |
|---|---|---|
| `CONGA_TOOL_PROXY` | 工具出站代理 | `socks5://127.0.0.1:1080` |

桌面版在顶栏 Globe 按钮中配置代理,优先级高于该环境变量;保存后下一次工具调用即生效,无需重启。该代理不影响 LLM API 请求(那部分继续用上面的 `CONGA_LLM_PROXY` 系列)。
点击 Disable 时若设置了 CONGA_TOOL_PROXY 则回退到该环境变量，而非直连。
远程 MCP server 的 HTTP 流量同样遵循 `CONGA_TOOL_PROXY`(兜底 `CONGA_LLM_PROXY`/`HTTPS_PROXY`)。

注意 fail-open 语义:`CONGA_TOOL_PROXY` 里的无效 URL(拼错、不支持 scheme)不会报错中断,只会打一条 warn 日志然后**静默回退直连**。桌面版 UI 保存时会做完整校验,坏值进不了配置;环境变量没有这道关卡,若你在意"绝不经由直连暴露流量"(例如绕封锁场景),请自行确认该变量值有效。

**fetch 内网防护(SSRF guard)**:`fetch` 工具默认拒绝访问非公网地址——IP 直连(127.x、10.x、172.16-31.x、192.168.x、169.254.x、`::1` 等)与解析到这些网段的主机名一律拦截,防止模型借工具探测内网或云元数据端点(如 `http://169.254.169.254/`)。例外:

| 变量 | 说明 | 示例 |
|---|---|---|
| `CONGA_FETCH_ALLOW_PRIVATE_NET` | 置 `1`/`true` 放行内网目标(信任的自托管局域网) | `1` |

配置了 `CONGA_TOOL_PROXY`(或桌面版代理)时,该防护整体跳过——出站走向由代理决定。

> **边界假设(排错时有用)**:防护的手段是「先解析一次、任一地址非公网即拒绝,再用 `resolve_to_addrs` 把连接钉死在校验过的地址上」。它约束的是 conga 自己的解析。如果链路上存在**透明代理**(不设任何 `*_PROXY` 环境变量也会拦截的那种),请求会被代理接管并用代理自己的 DNS 重新解析,上述钉死就失效了,此时 `fetch` 可能拿到代理返回的 `502 upstream connect failed` 而并非目标响应。这是部署环境属性而非代码缺陷;若你的网络里存在透明代理,请不要把 SSRF guard 当作唯一防线。
>
> 附带影响:依赖「本机回环直连」的测试(如 `pinned_client_connects_to_pinned_address_not_dns`)在此类网络下会失败。

---

## 4. 运行后端

### 4.1 网关服务器 `conga-gateway`(给 Web/桌面端用)

```bash
cd conga
cargo run --release --bin conga-gateway
```

- 默认监听 `127.0.0.1:3000`(`CONGA_GATEWAY_HOST` / `CONGA_GATEWAY_PORT` 可改)。

> **只监听回环是有意的**:网关驱动的是带 `bash` 工具的完整 agent,任何能连上端口的人都能以你的身份执行代码。默认绑定回环意味着只有本机可访问;需要局域网 / 容器访问时用 `CONGA_GATEWAY_HOST=0.0.0.0` 显式放开(此时启动会打一条 warning)。Docker 镜像已内置 `CONGA_GATEWAY_HOST=0.0.0.0`,因为容器内由 `-p` 决定暴露范围。

**鉴权(必读)**:网关驱动的是一个带 `bash` 工具的完整 agent,任何能连上端口的人都能以你的身份执行代码。因此 `/ws` 与全部 `/api/*` 都要求携带网关 token(`Authorization: Bearer <token>` 或 `?token=<token>`,浏览器 WebSocket 无法带 header 故两者皆可)。token 解析顺序:

1. `CONGA_GATEWAY_TOKEN` 环境变量(设置即用,不落盘);
2. `~/.conga/gateway_token`——首次启动自动生成(64 位十六进制,`0600` 权限),稳定复用。

浏览器前端在 **Settings → Connection** 粘贴 token(保存在本机,桌面端走 IPC 不需要)。

- 暴露:WebSocket `/ws`、REST `/api/commands`、`/api/sessions`、`GET /api/sessions/search?q=…`(FTS5 跨会话全文检索;每进程首个请求先增量重建 `~/.conga/index.db` 索引)、`/api/sessions/{key}/context`、`/api/sessions/{key}/context/compact`、`/api/sessions/{key}/messages`(后端真相端点:对磁盘 `events.jsonl` 跑 `derive_messages`,未知 key→404、损坏日志→500)。
- **会话存储**:每个会话是 `~/.conga/sessions/<id>/events.jsonl` 的一份**崩溃安全事件日志**——一轮里每个已发生的事实(助手消息、工具结果)在它发生时就落盘,而非等到整轮成功才追加;崩溃 / 失败 / 中断的轮次仍保有其已经发生的全部副作用。旧 `messages.jsonl` 会话首次打开时自动迁移并删除旧文件(不可逆)。详见 [架构 §5.5](./architecture.md) 与 [ADR 0001](./adr/0001-event-sourced-session-log.md)。

### 4.2 终端 REPL `conga`(纯命令行)

```bash
cd conga
cargo run --release --bin conga
# 带选项:
cargo run --release --bin conga -- --mode=full-auto --resume=last
```

- 启动后进入交互式 REPL,每行输入触发一轮对话。
- **启动参数**:`--mode=<suggest|auto-edit|full-auto|plan>`(默认 `auto-edit`)、`--resume=<id|last>`(恢复会话)。
- **斜杠命令**(输入 `/` 开头):

| 命令 | 作用 |
|---|---|
| `/help` | 列出命令 |
| `/mode <suggest\|auto-edit\|full-auto\|plan>` | 切换权限模式 |
| `/resume [id\|last]` | 恢复会话(默认 last) |
| `/clear` | 开新会话 |
| `/sessions` | 列出会话 |
| `/reload-tools` | 重新加载外部工具 |
| `/exit` | 退出 |

- **Ctrl-C**:在流式输出中触发**协作式中止**(在下一个安全点退出,返回已生成的部分);在输入行是 reedline 按键事件。
- **工具审批**:取决于模式与工具风险,可能弹出 `[approve <tool>? y/N]`,输入 `y` 放行。

#### Plan 模式(只读规划)

`--mode=plan` 或 `/mode plan` 进入:与 `suggest` 同为只读门控(Low 风险工具放行,write/edit/bash/fetch/subagents 一律阻断且**不询问审批**),同时在每轮用户消息尾部注入规划指令——要求 agent 用只读工具勘察仓库后**以文本输出实施计划**(改哪些文件、方案、风险、验证方式)。指令随消息持久化(与 environment 快照同一通道),系统提示词保持字节稳定,不破坏 provider 缓存前缀;真正的强制力来自权限门控,指令只是引导。

### 4.2.1 无头执行 `conga exec`(CI / 脚本)

```bash
./target/release/conga exec [--json] [--mode=<suggest|auto-edit|full-auto|plan>] [--resume=<id|last>] "<task>"
echo "fix the lint errors" | ./target/release/conga exec -
```

- **一轮,无 REPL**:与 REPL 共用同一套 Host 装配(`SessionAssembly::build_cli`)与 `run_turn`,行为完全一致。
- **默认 `--mode=full-auto`**:无头环境没有审批人,任何需要审批的工具都会被拒(stderr 提示);想更保守用 `--mode=plan`。
- **`--json`**:stdout 每行一个 NDJSON 事件,**与 gateway/桌面端同一种 wire 协议**(`event_to_ws` → `OutgoingEvent`);非 JSON 模式复用 REPL 的 `EventPrinter` 人读输出。会话 id、审批拒绝、子代理日志走 stderr,保证 stdout 可机器解析。
- **退出码**:`0` = 轮完成;`1` = 轮出错;`130` = 用户中断(SIGINT 惯例);`2` = 用法/装配错误。
- **`-` 读 stdin**:管道传入任务文本,便于脚本组合。

### 4.3 编译内置扩展(可选)

CLI 默认不带进程内扩展。需要 `hello`/`todo`/`search`/`permission_gate` 时,启用 feature:

```bash
cargo run --release --bin conga --features ext
```

### 4.4 测试与质量门禁

```bash
cd conga
cargo test --all-features
cargo fmt --all -- --check
cargo clippy --all-features -- -D warnings
```

---

## 5. Web 端(浏览器)

前端位于 `web/`,pnpm 管理。

```bash
cd web
pnpm install

# 开发模式(Vite dev server,默认端口 1420,带 HMR)
pnpm dev

# 生产构建(先 vue-tsc 类型检查,再 vite build → dist/)
pnpm build

# 预览生产构建
pnpm preview
```

### 5.1 指向后端网关

前端连接地址由 env 控制。编辑 `web/.env`(模板见 `web/.env.example`):

| 变量 | 默认 | 说明 |
|---|---|---|
| `VITE_WS_URL` | `ws://localhost:3000` | WebSocket 地址(流式对话) |
| `VITE_API_URL` | `http://localhost:3000` | REST 地址(上下文元数据) |

> 联调时:先起后端 `cargo run --bin conga-gateway`,前端 `VITE_WS_URL` 指向它;二者默认端口已对齐(3000)。

### 5.2 两种部署形态

- **前后端同源(推荐)**:`pnpm build` 出 `dist/`,让 `conga-gateway` 托管它(见 §4.1)。浏览器访问 `http://<gateway>:3000/` 即用,无跨域、无单独前端服务器。
- **前后端分离**:单独跑 `pnpm dev` / 部署 `dist/` 到任意静态服务器,`VITE_WS_URL` 指向后端网关(网关已放开 CORS)。

---

## 6. Tauri 桌面端

桌面端用同一份 `web/src`,Tauri 把 Vite 产物装进原生窗口。

```bash
cd web
pnpm install

# 开发模式(自动调 pnpm dev,连 localhost:1420)
pnpm tauri:dev      # = tauri dev

# 构建分发包(自动调 pnpm build,产 .dmg/.msi/.exe)
pnpm tauri:build    # = tauri build
```

- 产物:`web/src-tauri/` 配置中 `productName=Conga`、`identifier=com.conga.desktop`、`bundle.targets=all`。macOS 出 `.dmg`,Windows 出 `.msi`/`.exe`。
- 配置见 `web/src-tauri/tauri.conf.json`:`frontendDist=../dist`、`devUrl=http://localhost:1420`。

> **桌面端是自包含的**:Tauri 桌面端内置进程内 Host(`src-tauri/src/chat.rs`),通过 IPC 直接做推理,不需要独立 gateway。但仍需 LLM API key 和 `~/.conga` 配置(与 gateway 共用同一套)。浏览器版则需要独立部署的 gateway。

---

## 7. Docker 部署

仓库根 `Dockerfile` 是可用的多阶段构建:构建阶段编译 Rust workspace 全部 5 个 crate,运行阶段 `EXPOSE 3000`、`ENTRYPOINT ["conga-gateway"]`。镜像只包含网关(`/ws` + `/api/*`),Web 前端需单独部署(如 `pnpm build` 后用任意静态服务器托管 `web/dist`,并通过 `VITE_WS_URL`/`VITE_API_URL` 指向网关,来源加入 `CONGA_GATEWAY_CORS_ORIGINS`)。

```bash
docker build -t conga .
docker run -d -p 3000:3000 \
  -e CONGA_LLM_BASE_URL=https://api.deepseek.com/v1 \
  -e CONGA_LLM_KEY=sk-... \
  -e CONGA_LLM_MODEL=deepseek-chat \
  -e CONGA_LLM_API=openai \
  --name conga conga:latest
# 网关 API: http://localhost:3000/api/... (前端需另行部署)
```

运行时通过 `-e` 或 `--env-file` 注入 `CONGA_LLM_*` 等环境变量(完整清单见 §10)。

---

## 8. 上下文压缩配置

压缩在喂给 LLM 前缩小工作 transcript(日志只追加、压缩以一条 `Compacted` 检查点追加、无 LLM 摘要)。详见 [架构设计 §9](./architecture.md)。相关环境变量:

| 变量 | 默认 | 说明 |
|---|---|---|
| `CONGA_CONTEXT_WINDOW` | `128000` | 模型上下文窗口(token);设置对话框的 **Max tokens**(settings.json `maxTokens`)优先于它 |
| `CONGA_COMPACT_THRESHOLD_PCT` | `80` | 占用超过窗口的该百分比时触发压缩 |
| `CONGA_COMPACT_TARGET_PCT` | `50` | 压缩后目标占窗口的百分比(带滞后,防抖) |
| `CONGA_COMPACT_MAX_MESSAGES` | `80` | 无 provider usage 数据时,按消息条数兜底的阈值;`0` 表示不压缩 |

Web 端可在头部点 **Compress** 按钮手动触发(调用 `POST /api/sessions/{key}/context/compact`,见 §4.1)。

---

## 9. 工具与权限

### 9.1 内置工具

`read` / `write` / `edit` / `bash` / `grep` / `list` / `fetch`(详见 [架构设计 §5.2](./architecture.md))。每个工具自带风险等级:`read`/`grep`/`list`/`fetch` 为低风险,`write`/`edit` 为中风险,`bash` 为高风险。默认 `auto-edit` 模式下低/中风险自动放行,仅高风险(`bash`)请求审批。`fetch` 工具抓取 URL 并把 HTML 转成可读文本(markdown 风格),支持 http/https。`bash`/`fetch` 超过 200KB 的输出会完整落盘到 `~/.conga/sessions/<会话>/tool_state/<工具>/spill/`,上下文中只保留头部预览与文件路径(完整输出保留在磁盘上该路径,用户可自行查看或经 shell 取回)。落盘路径位于 `~/.conga` 之下,模型也可通过 `read` 工具以该绝对路径直接读回完整输出。`terminal` 工具位于 conga-ext,默认关闭:主机在 conga-ext 依赖上启用 `terminal` feature 后经其扩展注册入口生效(桌面端已启用;CLI 随 `--features ext` 一并启用)。它通过 PTY 运行命令:action=`run` 启动(同名 session 存活时旧进程被发送 SIGHUP 并回收;忽略 SIGHUP 的进程可能存活,属已知限制)、`read` 排空新输出并报告退出状态、`send` 向运行中进程的 stdin 写入一行,适合驱动交互式程序;会话按 `session` 参数(默认 `default`)区分,输出经 64KiB 环形缓冲按需排空、超限丢弃最旧输出(与 `bash`/`fetch` 的 200KB 落盘 spill 不同,该工具不落盘)。

### 9.2 外部工具(白名单)

通过环境变量 `CONGA_EXTERNAL_TOOLS`(逗号分隔的命令白名单)接入外部命令工具,启动时加载;CLI 里可用 `/reload-tools` 热重载。

```bash
# 例:允许把 rg、jq 作为工具暴露给 agent
CONGA_EXTERNAL_TOOLS=rg,jq
```

### 9.3 MCP 工具服务器

[Model Context Protocol](https://modelcontextprotocol.io)(MCP)是一个开放协议,生态里有大量现成工具服务器(GitHub、文件系统、数据库、浏览器、Slack…)。conga 作为 MCP 客户端,把这些 server 暴露的 tools 接进来,与内置工具同列供 agent 调用。

**配置文件**:`~/.conga/mcp.json`(或用 `$CONGA_MCP_CONFIG` 指定路径)。文件不存在 = 不加载任何 MCP 工具(静默,不报错)。格式与 Claude Desktop、Cline 等主流客户端一致,可直接复用现有配置。支持两种传输方式,可在同一配置文件中混用:

#### stdio 传输(本地子进程)

```json
{
  "mcpServers": {
    "github": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-github"],
      "env": { "GITHUB_PERSONAL_ACCESS_TOKEN": "ghp_xxx" }
    },
    "filesystem": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/home/user/docs"]
    }
  }
}
```

每个 server 是 `mcpServers` map 里的一项,key 即 server 名(用于工具名前缀),`command` + `args` 指定如何启动子进程,`env` 里的环境变量会**追加**到子进程(不替换父进程环境)。

#### Streamable HTTP 传输(远程服务器)

```json
{
  "mcpServers": {
    "remote-sentry": {
      "url": "https://mcp.sentry.dev/mcp",
      "headers": { "Authorization": "Bearer your-token-here" }
    }
  }
}
```

`url` 指向远程 MCP server 的 HTTP 端点,`headers` 里的键值对会作为 HTTP 请求头随每个 JSON-RPC POST 发送(常用于 `Authorization: Bearer ...`)。stdio(`command`)和 HTTP(`url`)在同一 server 条目中互斥,但不同 server 可以混用两种传输。

**工具命名**:MCP 工具名加前缀 `mcp__{server名}__{工具名}`(如 `mcp__github__create_issue`),避免与内置工具或跨 server 重名。所有 MCP 工具的**风险等级统一为 High**——在非 full-auto 模式下会请求审批。

**工作原理**:
- **stdio**:conga 为每个配置项 spawn 子进程,运行 MCP `initialize` 握手 → `tools/list` 发现工具 → 每个 MCP tool 包装成一个 `ToolDefinition`。Agent 调用工具时,conga 发送 `tools/call`,server 返回结果(text/image 内容)。子进程在工具被 drop 时自动终止。
- **Streamable HTTP**:conga 向 server URL POST JSON-RPC 请求,响应可能是单个 JSON 或 SSE 流(`text/event-stream`)。无状态会话——每个请求独立 POST。支持 `CONGA_LLM_PROXY` / `HTTPS_PROXY` 代理。

**支持范围**(当前版本):

- 传输:**stdio**(子进程)+ **Streamable HTTP**(远程服务器)。
- 协议:**legacy era**(`initialize` 握手,协议版本 `2025-06-18`)。覆盖现存几乎所有 MCP server。
- 原语:仅 **tools**。不接 resources / prompts / sampling / elicitation。
- 内容:text + image。未知类型降级为文本描述(不丢信息)。
- 超时:单次 `tools/call` 超时由 `CONGA_MCP_CALL_TIMEOUT_S` 控制(默认 60 秒)。

> **安全提示**:`mcp.json` 可能含 API key 等密文。确保该文件不被提交到版本控制(`~/.conga/` 默认在 home 目录外,不受仓库 gitignore 影响)。

### 9.4 搜索扩展

启用 `ext` feature 后,`search` 工具支持多家 provider,由 `CONGA_SEARCH_PROVIDER` 选择,并填对应 key:

| 变量 |
|---|
| `CONGA_SEARCH_PROVIDER` |
| `CONGA_BRAVE_API_KEY` / `CONGA_TAVILY_API_KEY` / `CONGA_SERPER_API_KEY` / `CONGA_SERPAPI_API_KEY` / `CONGA_EXA_API_KEY` / `CONGA_FIRECRAWL_API_KEY` |

### 9.5 权限模式

| 模式 | 行为 |
|---|---|
| `suggest` | 只读:低风险放行,中/高风险直接拒绝 |
| `auto-edit` | 低/中风险自动放行,高风险请求审批(**CLI 默认**,gateway 默认 `auto-edit`) |
| `full-auto` | 全部自动放行(慎用) |

入口设定:CLI 用 `--mode=` 或 `/mode`;gateway 用 `CONGA_GATEWAY_MODE`;审批超时用 `CONGA_APPROVAL_TIMEOUT_S`(默认 300 秒)。

### 9.6 bash 沙箱(CONGA_SANDBOX)

设置 `CONGA_SANDBOX=1` 后,`bash` 工具的命令在操作系统级文件系统沙箱中执行:macOS 使用 `sandbox-exec`(Seatbelt,系统自带);Linux 使用 Landlock,要求以 `--features sandbox-landlock` 构建否则 `CONGA_SANDBOX=1` fail-closed 并附带 rebuild 提示。写操作仅允许在当前工作目录、`TMPDIR` 与 `/var/tmp` 内,其余路径只读。未设置时(默认)行为完全不变。

**沙箱是 fail-closed 的**:如果隔离无法施加,命令会被直接拒绝并返回错误,而不是降级放行。macOS 上首次使用时会自检 `sandbox-exec` 是否真的能应用含 `deny` 规则的 profile——Apple 已在新版 macOS 上削弱该命令:纯 `allow` 的 profile 仍生效,但任何 `deny` 规则会被 `sandbox_apply: Operation not permitted` 拒绝(退出码 71)。自检失败时工具报错指出「沙箱不可用」,而不是让命令带着失效的沙箱继续执行。

**能力边界(重要)**:沙箱只约束**文件写入**。网络出口、任意文件读取、进程 exec 均不受限制——`cat ~/.ssh/id_rsa` 后外传这类「读 + 外传」链路在 `CONGA_SANDBOX=1` 下依然通畅。因此它应被当作防误伤的护栏(挡住 `rm -rf`、挡住写到项目目录之外),而不是对抗恶意代码的边界;真正需要隔离时请使用容器或虚拟机。

### 9.7 Skills(可选)

技能是磁盘上的 Markdown 指令文件。启动时 conga 只把「名称 + 描述 + 文件路径」目录追加到系统提示;模型需要某个技能时,用 `read` 工具按目录给出的路径读取全文。全文不进入系统提示,不占用上下文预算。描述超过 200 字符会被截断(目录随每个请求付费,这是护栏)。子代理(subagent)提示不参与技能目录。

两个存放位置(同名时项目覆盖全局):

- 全局:`~/.conga/skills/*.md`(目录里给绝对路径,`read` 已允许读取 `~/.conga` 内的绝对路径)
- 项目:`<项目根>/.conga/skills/*.md`(目录里给相对项目根的路径,`read` 在项目根内解析相对路径)

文件必须以 YAML frontmatter 开头,`name` 与 `description` 缺一不可,否则该文件被跳过并在日志告警。`description` 支持单行标量与块标量(`|`/`>`,含 `-`/`+` chomping 指示);块标量会被折叠成一行进目录。文件需使用 LF(Unix)换行符——CRLF 文件无法通过严格的 `---\n` frontmatter 校验,同样会被跳过并在日志告警。示例 `~/.conga/skills/commit-style.md`:

```markdown
---
name: commit-style
description: Enforce conventional commit messages with scope
---
Write commit titles as `type(scope): summary`, lowercase, imperative mood...
```

---

> **服务器宿主(gateway / 桌面端)不会运行在它们服务的项目里** —— 项目技能与工具沙箱跟随 `CONGA_PROJECT_DIR`(见 §10),不设置时退化为进程 cwd。Web 端要用项目技能,在 gateway 的 `.env` 里设 `CONGA_PROJECT_DIR=<项目根>`。

## 10. 环境变量完整参考

> `conga/.env.example` 模板已覆盖常用变量,本表为完整参考。

### LLM 连接(必填三项)

| 变量 | 默认 | 说明 |
|---|---|---|
| `CONGA_LLM_BASE_URL` | — | provider 基础 URL(必填) |
| `CONGA_LLM_KEY` | — | API key(必填) |
| `CONGA_LLM_MODEL` | — | 模型 id(必填) |
| `CONGA_LLM_API` | `openai` | `openai` 或 `anthropic` |
| `CONGA_LLM_PROXY` / `CONGA_LLM_HTTP_PROXY` / `CONGA_LLM_HTTPS_PROXY` | — | 代理(见 §3.3) |

### 推理循环旋钮(均可选)

| 变量 | 默认 | 说明 |
|---|---|---|
| `CONGA_MAX_TURNS` | `50` | 外层循环最大轮数 |
| `CONGA_MAX_TOOL_CALLS` | `20` | 单轮内工具调用上限 |
| `CONGA_MAX_TOKENS` | `4096` | 模型输出 token 上限 |
| `CONGA_THINKING` | — | 已移除:该变量从未生效(没有任何 provider 读取),设置它不会有任何效果,可安全删除。 |
| `CONGA_RETRY_MAX` | `2` | LLM 调用最大重试次数(仅流前失败) |
| `CONGA_RETRY_MAX_MS` | `8000` | 退避上限(ms) |

### 网关服务器

| `CONGA_GATEWAY_HOST` | `127.0.0.1` | 监听地址。默认只绑回环——网关能执行 shell,放开到 `0.0.0.0` 等于把本机 shell 暴露给整个网络(Docker 镜像内置此值) |
| `CONGA_GATEWAY_PORT` | `3000` | 监听端口 |
| `CONGA_GATEWAY_CORS_ORIGINS` | `http://localhost:1420,http://127.0.0.1:1420` | 追加允许的跨域来源(逗号分隔)。前端始终独立部署(Vite 开发服务器或其他静态托管),把实际来源加入此列表 |
| `CONGA_GATEWAY_MODE` | `auto-edit` | 审批模式 `suggest`/`auto-edit`/`full-auto` |
| `CONGA_GATEWAY_TOKEN` | 自动生成 | 网关鉴权 token;未设置时首次启动生成 `~/.conga/gateway_token`(0600)。`/ws` 与 `/api/*` 必须携带(Bearer header 或 `?token=`) |
| `CONGA_APPROVAL_TIMEOUT_S` | `300` | 审批等待超时(秒) |
| `CONGA_PROJECT_DIR` | 进程 cwd | 项目根:工具沙箱边界与 `<dir>/.conga/skills` 项目技能扫描根(服务器宿主用,见 §9.7) |

### 上下文压缩

| 变量 | 默认 | 说明 |
|---|---|---|
| `CONGA_CONTEXT_WINDOW` | `128000` | 模型上下文窗口;settings.json `maxTokens` 优先于它 |
| `CONGA_COMPACT_THRESHOLD_PCT` | `80` | 触发压缩阈值(%) |
| `CONGA_COMPACT_TARGET_PCT` | `50` | 压缩后目标(%) |
| `CONGA_COMPACT_MAX_MESSAGES` | `80` | 条数兜底(`0`=不压缩) |

### 工具 / 搜索 / MCP

| 变量 | 说明 |
|---|---|
| `CONGA_EXTERNAL_TOOLS` | 外部命令工具白名单(逗号分隔,见 §9.2) |
| `CONGA_MCP_CONFIG` | MCP 配置文件路径(默认 `~/.conga/mcp.json`,见 §9.3) |
| `CONGA_SEARCH_PROVIDER` | 搜索 provider 选择(需 `ext` feature) |
| `CONGA_BRAVE_API_KEY` / `CONGA_TAVILY_API_KEY` / `CONGA_SERPER_API_KEY` / `CONGA_SERPAPI_API_KEY` / `CONGA_EXA_API_KEY` / `CONGA_FIRECRAWL_API_KEY` | 各搜索商 key |
| `CONGA_SANDBOX` | 置 1 时 bash 工具启用文件系统沙箱(见 §9.6) |

### 前端(`web/.env`)

| 变量 | 默认 | 说明 |
|---|---|---|
| `VITE_WS_URL` | `ws://localhost:3000` | WebSocket 地址 |
| `VITE_API_URL` | `http://localhost:3000` | REST 地址 |

---

## 11. 故障排查 / FAQ

| 现象 | 排查 |
|---|---|
| CLI 启动报 `config error` 并退出 | 三项必填 env 缺失。确认 `conga/.env` 有 `CONGA_LLM_BASE_URL`/`CONGA_LLM_KEY`/`CONGA_LLM_MODEL`,或在 shell 里 `export`。 |
| Web 端连不上、一直离线 | `VITE_WS_URL` 指向错或后端未起。确认 `conga-gateway` 在跑(默认 3000),且 `VITE_WS_URL=ws://localhost:3000`。重连 5 次后会显示手动 Reconnect 按钮。 |
| Web/桌面端用不了项目技能 | 服务器宿主的项目根由 `CONGA_PROJECT_DIR` 决定(不设 = 进程 cwd,通常不是你的项目)。在 gateway/桌面端的 `.env` 设 `CONGA_PROJECT_DIR=<项目根>`,并确认 `<项目根>/.conga/skills/*.md` 的 frontmatter 有 `name` 和 `description`。 |
| 端口 3000 被占用 | 用 `CONGA_GATEWAY_PORT=<其它端口>` 改网关端口,并把前端 `VITE_WS_URL`/`VITE_API_URL` 同步改掉。 |
| 报 `orphan tool_call` / 工具结果错乱 | 通常与压缩有关;确认没有手动设异常小的 `CONGA_COMPACT_MAX_MESSAGES`。正常情况下原子组会保护 tool_call↔result。 |
| 设了 `CONGA_THINKING` 没效果 | 正常:该变量已移除,从未有 provider 读取它(extended thinking 相关链路已整体删除)。从 `.env` 里删掉即可。 |
| 桌面端打不开/不响应 | 确认 LLM API key 已配置(环境变量或 `conga/.env`);桌面端通过进程内 Host 做 IPC 推理,不需要独立 gateway。 |
| 想用 Claude(Anthropic) | `CONGA_LLM_API=anthropic`,`CONGA_LLM_BASE_URL=https://api.anthropic.com/v1`,`CONGA_LLM_MODEL=claude-...`。 |
| 想接本地 Ollama/vLLM | `CONGA_LLM_API=openai`(默认),`CONGA_LLM_BASE_URL=http://localhost:11434/v1`(Ollama 示例),key 随意填。 |
| MCP server 启动失败 / 工具不出现 | 确认 `command` 在 `PATH` 里(如 `npx`);检查 `~/.conga/mcp.json` JSON 合法;server 自身的 `env`(API key)正确。单个 server 失败不影响其他 server 和内置工具。 |

---

## 12. 速查

```bash
# 后端网关(托管前端,一键起 Web 服务)
cd conga && cargo run --release --bin conga-gateway

# 终端 REPL
cd conga && cargo run --release --bin conga

# 前端开发(浏览器,1420)
cd web && pnpm install && pnpm dev

# 前端生产构建(交给网关托管)
cd web && pnpm build

# 桌面端
cd web && pnpm tauri:dev      # 开发
cd web && pnpm tauri:build    # 打包
```

> 进一步了解分层、数据流、工具系统、压缩算法与设计取舍,见 [架构设计](./architecture.md)。
