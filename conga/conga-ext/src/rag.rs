//! RAG tools over the personal conga-rag index.
//!
//! `rag_search` (read-only) retrieves from the vector store built by
//! `conga-rag ingest`; `rag_remember` appends (or overwrites by title) a
//! memory note under the built-in `memory` source and incrementally indexes
//! it. Files on disk are the source of truth; the store is derived. Hit
//! paths are printed absolute so the agent can follow up with the `read`
//! tool (检索 → 精读闭环).
//!
//! Both tools read the same config as the `conga-rag` CLI
//! (`$CONGA_RAG_CONFIG` → ./rag.toml → ~/.conga/rag.toml).

use std::sync::Arc;

use conga::{RiskLevel, ToolDefinition, ToolError, ToolResult};
use tracing::info;

/// Upper bound for the `k` argument (guards the context budget).
const MAX_K: usize = 20;
/// Cap per hit. Chunks are ~1.2k chars by default; this only guards against
/// pathological chunks blowing up the context.
const MAX_HIT_CHARS: usize = 1600;

/// `rag_remember`: append (or overwrite by title) a memory note under the
/// built-in `notes` source, then incrementally index it.
const MAX_TITLE_CHARS: usize = 80;
const MAX_CONTENT_CHARS: usize = 8_000;
const MAX_SLUG_CHARS: usize = 60;

/// CJK-safe filename slug: alphanumeric (incl. CJK) kept, everything else
/// collapses to '-'. Empty after trim → caller falls back to "note".
fn slugify(title: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in title.trim().chars() {
        if c.is_alphanumeric() {
            if dash && !out.is_empty() {
                out.push('-');
            }
            dash = false;
            out.push(c);
        } else {
            dash = true;
        }
    }
    out.chars().take(MAX_SLUG_CHARS).collect()
}

/// Memory notes root: `<builtin_base>/memory`, shared with conga-rag's
/// built-in `memory` source (and the prompt-appended memory catalog —
/// same frontmatter format). When built-ins are explicitly disabled
/// (CONGA_RAG_BUILTIN_BASE="") the write path still lands under the real
/// config dir so files never scatter.
fn memory_dir() -> std::path::PathBuf {
    conga_rag::config::builtin_base()
        .unwrap_or_else(conga::storage::config_dir)
        .join("memory")
}

pub fn register(api: &mut dyn conga::ExtensionApi) {
    api.register_tool(ToolDefinition {
        name: "rag_search".into(),
        label: "RAG Search".into(),
        description: "语义检索个人知识库(用户笔记/文档 + agent 长期记忆与蒸馏教训 memory,由 `conga-rag ingest` 或 `rag_remember` 维护)。适用:查找个人笔记、过往总结、项目文档、长期记忆。不适用:找代码文件(用 grep)、联网信息(用 web_search)。返回相关片段与源文件路径;需要全文时用 read 工具读取源文件。source 可选值含 notes/memory。".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "检索词(自然语言或关键词)" },
                "k": { "type": "number", "description": "返回条数,默认取配置 ask.top_k,上限 20" },
                "source": { "type": "string", "description": "可选,按源名过滤(如 notes)" }
            },
            "required": ["query"]
        }),
        risk: RiskLevel::Low,
        execute: Arc::new(move |ctx| {
            Box::pin(async move {
                if ctx.aborted() {
                    return Err(ToolError::Message("aborted".into()));
                }
                let query = ctx.args["query"].as_str().unwrap_or_default().trim().to_string();
                if query.is_empty() {
                    return Err(ToolError::Message("query 不能为空".into()));
                }
                let k_arg = ctx.args.get("k").and_then(|v| v.as_u64()).map(|n| n as usize);
                let source = ctx
                    .args
                    .get("source")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string);

                let (_cfg_path, cfg) = conga_rag::config::RagConfig::load()
                    .map_err(|e| ToolError::Message(format!("加载 RAG 配置失败:{e}")))?;
                // Reject a missing store before run_search: Store::open would
                // create an empty db file, and agent curiosity should not
                // litter the filesystem.
                let store = cfg.store_path();
                if !store.exists() {
                    return Err(ToolError::Message(format!(
                        "索引不存在:{}. 请先运行 conga-rag ingest 建立索引",
                        store.display()
                    )));
                }
                let k = k_arg.unwrap_or(cfg.ask.top_k).clamp(1, MAX_K);
                info!("rag_search: query='{}' k={} source={:?}", query, k, source);

                let hits = conga_rag::search::run_search(&cfg, &query, k, source.as_deref())
                    .await
                    .map_err(|e| ToolError::Message(format!("检索失败:{e}")))?;
                if hits.is_empty() {
                    return Ok(ToolResult::text(
                        "未找到相关片段。换个说法或更具体的关键词再试。",
                    ));
                }
                Ok(ToolResult::text(format_hits(&cfg, &hits)))
            })
        }),
    });
    api.register_tool(ToolDefinition {
        name: "rag_remember".into(),
        label: "RAG Remember".into(),
        description: "把跨会话有用的知识/偏好/事实写入长期记忆(同 title 覆盖旧记忆)。适用:用户说\"记住这个\",或你判断该知识未来会话仍需要。不适用:易变状态、一次性任务细节(不要记)。写入后可用 rag_search 检索。".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "title":   { "type": "string", "description": "记忆标题(主键,同题覆盖)" },
                "content": { "type": "string", "description": "自包含的知识陈述,脱离当前对话也能读懂" },
                "tags":    { "type": "array", "items": { "type": "string" }, "description": "可选标签" }
            },
            "required": ["title", "content"]
        }),
        risk: RiskLevel::Medium,
        execute: Arc::new(move |ctx| {
            Box::pin(async move {
                if ctx.aborted() {
                    return Err(ToolError::Message("aborted".into()));
                }
                let title = ctx.args["title"]
                    .as_str()
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                let content = ctx.args["content"]
                    .as_str()
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                let tags: Vec<String> = ctx
                    .args
                    .get("tags")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|t| t.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                if title.is_empty() || title.chars().count() > MAX_TITLE_CHARS {
                    return Err(ToolError::Message(format!(
                        "title 不能为空且不超过 {MAX_TITLE_CHARS} 字符"
                    )));
                }
                if content.is_empty() || content.chars().count() > MAX_CONTENT_CHARS {
                    return Err(ToolError::Message(format!(
                        "content 不能为空且不超过 {MAX_CONTENT_CHARS} 字符"
                    )));
                }

                // Config must resolve before any disk write: a missing
                // rag.toml fails loud with zero filesystem side effects.
                let (_cfg_path, mut cfg) = conga_rag::config::RagConfig::load()
                    .map_err(|e| ToolError::Message(format!("加载 RAG 配置失败:{e}")))?;
                let dir = memory_dir();
                std::fs::create_dir_all(&dir).map_err(|e| ToolError::Message(e.to_string()))?;
                let slug = {
                    let s = slugify(&title);
                    if s.is_empty() { "note".into() } else { s }
                };
                let path = dir.join(format!("{slug}.md"));
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0)
                    .to_string();
                let markdown = conga_host::memory::entry_markdown(
                    &title,
                    &tags,
                    &now,
                    &ctx.ctx.session_id,
                    &content,
                );
                std::fs::write(&path, markdown).map_err(|e| {
                    ToolError::Message(format!("写入失败 {}: {e}", path.display()))
                })?;
                // The memory dir may have just been created; make sure this
                // ingest run sees the built-in source (idempotent).
                cfg.inject_builtins();
                info!("rag_remember: title='{}' -> {}", title, path.display());
                match conga_rag::pipeline::run_ingest(&cfg, Some("memory"), false).await {
                    Ok(_) => Ok(ToolResult::text(format!(
                        "已记住:{title}\n文件:{}\n(同 title 再次写入会覆盖;用 rag_search 检索)",
                        path.display()
                    ))),
                    Err(e) => Ok(ToolResult::error(format!(
                        "文件已写入 {} 但索引失败:{e}(下次 ingest 会自动补偿)",
                        path.display()
                    ))),
                }
            })
        }),
    });
}

/// Absolute path for `read`: `run_search` returns source-root-relative paths
/// (CLI display convention), so rejoin with the configured source root.
/// A path that is already absolute (source root unknown / not a prefix) is
/// shown as-is.
fn display_path(cfg: &conga_rag::config::RagConfig, h: &conga_rag::store::Hit) -> String {
    let p = std::path::Path::new(&h.path);
    if p.is_absolute() {
        return h.path.clone();
    }
    match cfg.sources.get(&h.source) {
        Some(src) => src.path.join(p).to_string_lossy().into_owned(),
        None => h.path.clone(),
    }
}

/// Char-safe truncation (never slices through a multi-byte char).
fn cap_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push_str("\n…(片段过长已截断)");
    out
}

fn format_hits(cfg: &conga_rag::config::RagConfig, hits: &[conga_rag::store::Hit]) -> String {
    let model = cfg
        .resolve_embedding()
        .map(|r| r.model)
        .unwrap_or_else(|_| "?".into());
    let mut out = format!("找到 {} 条相关片段(索引模型 {}):\n\n", hits.len(), model);
    for (i, h) in hits.iter().enumerate() {
        out.push_str(&format!(
            "[{}] score={:.3} {} {} (片段 {})\n",
            i + 1,
            h.score,
            h.source,
            display_path(cfg, h),
            h.ordinal + 1
        ));
        out.push_str(&cap_chars(&h.content, MAX_HIT_CHARS));
        out.push_str("\n\n");
    }
    out.push_str("提示:完整文件可用 read 工具读取;索引基于上次 ingest,最新内容以磁盘为准。");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use conga::{ContentBlock, ExtensionApiImpl, ToolCallCtx};
    use std::sync::atomic::AtomicBool;

    /// CONGA_RAG_CONFIG is process-global; serialize the tests that touch it.
    /// tokio Mutex in a OnceLock (its `new` is not const): the guard is held
    /// across the awaited tool call.
    static ENV_LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();

    async fn env_lock() -> tokio::sync::MutexGuard<'static, ()> {
        ENV_LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await
    }

    fn make_ctx(args: serde_json::Value) -> ToolCallCtx {
        ToolCallCtx {
            tool_call_id: "t".into(),
            args,
            signal: Arc::new(AtomicBool::new(false)),
            ctx: conga::ToolContext {
                cwd: std::env::temp_dir(),
                env: Default::default(),
                session_id: "test".into(),
                state_dir: std::env::temp_dir(),
            },
        }
    }

    fn registered_tool() -> ToolDefinition {
        registered_tool_named("rag_search")
    }

    fn registered_tool_named(name: &str) -> ToolDefinition {
        let mut api = ExtensionApiImpl::new();
        register(&mut api);
        api.tools
            .into_iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("{name} registered"))
    }

    fn text_of(res: ToolResult) -> String {
        match res.content.first() {
            Some(ContentBlock::Text { text }) => text.clone(),
            other => panic!("expected text block, got {other:?}"),
        }
    }

    #[test]
    fn register_exposes_rag_search_with_low_risk() {
        let tool = registered_tool();
        // Security invariant: the tool is read-only, so Low is correct —
        // and it must never be raised to auto-approve-worthy risk silently.
        assert!(matches!(tool.risk, RiskLevel::Low));
        assert!(tool.parameters["properties"]["query"].is_object());
        assert_eq!(tool.parameters["required"][0], "query");
    }

    #[tokio::test]
    async fn tool_searches_mock_index_end_to_end() {
        let _g = env_lock().await;
        let notes = tempfile::tempdir().unwrap();
        let dbdir = tempfile::tempdir().unwrap();
        std::fs::write(
            notes.path().join("架构.md"),
            "架构分层设计要点 match target",
        )
        .unwrap();
        std::fs::write(notes.path().join("其他.md"), "completely unrelated").unwrap();
        let (base, _req) = conga_rag::testsupport::spawn_mock_embeddings(0).await;
        let cfg_path = dbdir.path().join("rag.toml");
        std::fs::write(
            &cfg_path,
            format!(
                r#"[sources.notes]
kind = "dir"
path = {:?}
include = ["**/*.md"]
exclude = []

[embedding]
base_url = {:?}
api_key = "k"
model = "mock"
batch = 4

[store]
path = {:?}
"#,
                notes.path(),
                base,
                dbdir.path().join("t.db")
            ),
        )
        .unwrap();
        std::env::set_var("CONGA_RAG_CONFIG", &cfg_path);
        std::env::set_var("CONGA_RAG_BUILTIN_BASE", "");

        let (_p, cfg) = conga_rag::config::RagConfig::load().unwrap();
        conga_rag::pipeline::run_ingest(&cfg, None, false)
            .await
            .unwrap();

        let tool = registered_tool();
        let res = (tool.execute)(make_ctx(serde_json::json!({ "query": "match", "k": 2 })))
            .await
            .expect("tool succeeds");
        std::env::remove_var("CONGA_RAG_CONFIG");
        std::env::remove_var("CONGA_RAG_BUILTIN_BASE");

        let text = text_of(res);
        assert!(text.contains("找到"), "{text}");
        assert!(text.contains("match target"), "{text}");
        assert!(
            text.contains("架构.md"),
            "absolute-ish source path shown: {text}"
        );
        assert!(text.contains("read 工具"), "follow-up hint present: {text}");
    }

    #[tokio::test]
    async fn missing_store_errors_before_search() {
        let _g = env_lock().await;
        let notes = tempfile::tempdir().unwrap();
        let dbdir = tempfile::tempdir().unwrap();
        let (base, _req) = conga_rag::testsupport::spawn_mock_embeddings(0).await;
        let cfg_path = dbdir.path().join("rag.toml");
        std::fs::write(
            &cfg_path,
            format!(
                r#"[sources.notes]
kind = "dir"
path = {:?}
include = ["**/*.md"]
exclude = []

[embedding]
base_url = {:?}
api_key = "k"
model = "mock"

[store]
path = {:?}
"#,
                notes.path(),
                base,
                dbdir.path().join("never-built.db")
            ),
        )
        .unwrap();
        std::env::set_var("CONGA_RAG_CONFIG", &cfg_path);
        std::env::set_var("CONGA_RAG_BUILTIN_BASE", "");

        let tool = registered_tool();
        let res = (tool.execute)(make_ctx(serde_json::json!({ "query": "x" }))).await;
        std::env::remove_var("CONGA_RAG_CONFIG");
        std::env::remove_var("CONGA_RAG_BUILTIN_BASE");

        let err = res.expect_err("store missing must fail");
        assert!(err.to_string().contains("conga-rag ingest"), "{err}");
    }

    #[tokio::test]
    async fn missing_config_fails_loud() {
        let _g = env_lock().await;
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("CONGA_RAG_CONFIG", tmp.path().join("no-such-rag.toml"));
        std::env::set_var("CONGA_RAG_BUILTIN_BASE", "");

        let tool = registered_tool();
        let res = (tool.execute)(make_ctx(serde_json::json!({ "query": "x" }))).await;
        std::env::remove_var("CONGA_RAG_CONFIG");
        std::env::remove_var("CONGA_RAG_BUILTIN_BASE");

        let err = res.expect_err("config missing must fail");
        assert!(err.to_string().contains("CONGA_RAG_CONFIG"), "{err}");
    }

    #[tokio::test]
    async fn empty_query_is_rejected() {
        let tool = registered_tool();
        let res = (tool.execute)(make_ctx(serde_json::json!({ "query": "  " }))).await;
        assert!(res.is_err(), "blank query rejected without touching config");
    }

    #[test]
    fn cap_chars_is_char_safe() {
        let s = "中".repeat(2000);
        let out = cap_chars(&s, MAX_HIT_CHARS);
        // head + one-line truncation suffix; char-boundary slicing must not
        // panic on multi-byte input.
        assert!(out.chars().count() <= MAX_HIT_CHARS + 20);
        assert!(out.contains("…"));
        // Short input passes through untouched.
        let short = "短".repeat(10);
        assert_eq!(cap_chars(&short, MAX_HIT_CHARS), short);
    }

    // --- rag_remember ---

    #[test]
    fn register_exposes_rag_remember_with_medium_risk() {
        let tool = registered_tool_named("rag_remember");
        // 写文件的工具必须与 write 同级:AutoEdit 放行,Suggest/Plan 需确认。
        assert!(matches!(tool.risk, RiskLevel::Medium));
        assert_eq!(tool.parameters["required"][0], "title");
        assert_eq!(tool.parameters["required"][1], "content");
    }

    #[test]
    fn slugify_keeps_cjk_and_collapses_separators() {
        assert_eq!(slugify("  架构 分层!设计 "), "架构-分层-设计");
        assert_eq!(slugify("A/B\\C:D"), "A-B-C-D");
        assert!(slugify("!!!").is_empty());
    }

    #[tokio::test]
    async fn remember_rejects_blank_and_oversize_args() {
        let tool = registered_tool_named("rag_remember");
        for bad in [
            serde_json::json!({ "title": "  ", "content": "x" }),
            serde_json::json!({ "title": "t", "content": "" }),
            serde_json::json!({ "title": "t", "content": "中".repeat(MAX_CONTENT_CHARS + 1) }),
            serde_json::json!({ "title": "字".repeat(MAX_TITLE_CHARS + 1), "content": "x" }),
        ] {
            let err = (tool.execute)(make_ctx(bad.clone())).await;
            assert!(err.is_err(), "must reject: {bad}");
        }
    }

    fn remember_args(title: &str, content: &str) -> serde_json::Value {
        serde_json::json!({ "title": title, "content": content, "tags": ["t"] })
    }

    #[tokio::test]
    async fn remember_then_search_round_trip_and_overwrite() {
        let _g = env_lock().await;
        let base = tempfile::tempdir().unwrap(); // CONGA_RAG_BUILTIN_BASE
        let dbdir = tempfile::tempdir().unwrap();
        let dummy = tempfile::tempdir().unwrap(); // 占位 user source
        std::fs::create_dir_all(base.path().join("memory")).unwrap();
        let (emb, _req) = conga_rag::testsupport::spawn_mock_embeddings(0).await;
        let cfg_path = dbdir.path().join("rag.toml");
        std::fs::write(
            &cfg_path,
            format!(
                r#"[sources.dummy]
type = "dir"
path = {:?}

[embedding]
base_url = {:?}
api_key = "k"
model = "mock"

[store]
path = {:?}
"#,
                dummy.path(),
                emb,
                dbdir.path().join("t.db")
            ),
        )
        .unwrap();
        std::env::set_var("CONGA_RAG_CONFIG", &cfg_path);
        std::env::set_var("CONGA_RAG_BUILTIN_BASE", base.path());

        let remember = registered_tool_named("rag_remember");
        let r = (remember.execute)(make_ctx(remember_args(
            "架构决策",
            "网关走单进程 embedded 模式 xyzzy",
        )))
        .await
        .expect("first remember ok");
        assert!(text_of(r).contains("已记住"));
        assert!(
            base.path().join("memory/架构决策.md").exists(),
            "frontmatter 文件落盘"
        );

        // 同 title 覆盖:单文档、内容更新
        (remember.execute)(make_ctx(remember_args(
            "架构决策",
            "改主意了,网关拆双进程 quux",
        )))
        .await
        .unwrap();
        {
            let (_p, cfg) = conga_rag::config::RagConfig::load().unwrap();
            let store = conga_rag::store::Store::open(&cfg.store_path())
                .await
                .unwrap();
            let docs = store.docs_for_source("memory").await.unwrap();
            assert_eq!(docs.len(), 1, "同 title 覆盖,不是追加");
            store.close().await.unwrap();
        }

        let search = registered_tool_named("rag_search");
        let res = (search.execute)(make_ctx(
            serde_json::json!({ "query": "quux", "source": "memory" }),
        ))
        .await
        .expect("search ok");
        std::env::remove_var("CONGA_RAG_CONFIG");
        std::env::remove_var("CONGA_RAG_BUILTIN_BASE");
        let text = text_of(res);
        assert!(
            text.contains("quux") && !text.contains("xyzzy"),
            "检索到的是覆盖后内容: {text}"
        );
    }

    #[tokio::test]
    async fn remember_reports_partial_success_when_ingest_fails() {
        let _g = env_lock().await;
        let base = tempfile::tempdir().unwrap();
        let dummy = tempfile::tempdir().unwrap();
        let (emb, _req) = conga_rag::testsupport::spawn_mock_embeddings(0).await;
        std::fs::create_dir_all(base.path().join("memory")).unwrap();
        let cfg_path = base.path().join("rag.toml");
        std::fs::write(
            &cfg_path,
            format!(
                r#"[sources.dummy]
type = "dir"
path = {:?}

[embedding]
base_url = {:?}
api_key = "k"
model = "mock"

[store]
path = {:?}
"#,
                dummy.path(),
                emb,
                base.path().join("t.db")
            ),
        )
        .unwrap();
        // 让 store 无法打开:path 是目录
        std::fs::create_dir_all(base.path().join("t.db")).unwrap();
        std::env::set_var("CONGA_RAG_CONFIG", &cfg_path);
        std::env::set_var("CONGA_RAG_BUILTIN_BASE", base.path());

        let remember = registered_tool_named("rag_remember");
        let res = (remember.execute)(make_ctx(remember_args("p", "c")))
            .await
            .expect("tool returns Ok");
        std::env::remove_var("CONGA_RAG_CONFIG");
        std::env::remove_var("CONGA_RAG_BUILTIN_BASE");
        let text = text_of(res);
        assert!(
            text.contains("已写入") && text.contains("索引失败"),
            "部分成功如实上报: {text}"
        );
        assert!(base.path().join("memory/p.md").exists(), "文件确实落盘");
    }

    // --- memory_dir / display_path / format_hits ---

    /// A config with one directory source, so `display_path` has a root to
    /// join the source-relative hit paths with.
    fn cfg_with_source(name: &str, root: &std::path::Path) -> conga_rag::config::RagConfig {
        let mut cfg = conga_rag::config::RagConfig::default();
        cfg.sources.insert(
            name.to_string(),
            conga_rag::config::SourceConfig {
                kind: "dir".into(),
                path: root.to_path_buf(),
                include: vec!["**/*.md".into()],
                exclude: Vec::new(),
            },
        );
        cfg
    }

    fn hit(source: &str, path: &str, content: &str) -> conga_rag::store::Hit {
        conga_rag::store::Hit {
            source: source.into(),
            path: path.into(),
            ordinal: 0,
            content: content.into(),
            score: 0.5,
        }
    }

    #[tokio::test]
    async fn memory_dir_is_the_memory_folder_under_the_builtin_base() {
        let _g = env_lock().await;
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var("CONGA_RAG_BUILTIN_BASE", tmp.path());
        let dir = memory_dir();
        std::env::remove_var("CONGA_RAG_BUILTIN_BASE");
        assert_eq!(dir, tmp.path().join("memory"));
    }

    #[tokio::test]
    async fn memory_dir_falls_back_to_the_config_dir_when_builtins_are_disabled() {
        let _g = env_lock().await;
        // An empty builtin base means "builtins disabled": the write path
        // still has to land under the real config dir, never scattered.
        std::env::set_var("CONGA_RAG_BUILTIN_BASE", "");
        let dir = memory_dir();
        std::env::remove_var("CONGA_RAG_BUILTIN_BASE");
        assert_eq!(dir, conga::storage::config_dir().join("memory"));
        assert_eq!(dir.file_name().unwrap(), "memory");
    }

    #[test]
    fn display_path_joins_a_relative_hit_with_its_source_root() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_source("notes", tmp.path());
        let h = hit("notes", "sub/架构.md", "内容");
        assert_eq!(
            display_path(&cfg, &h),
            tmp.path()
                .join("sub/架构.md")
                .to_string_lossy()
                .into_owned()
        );
    }

    #[test]
    fn display_path_keeps_an_absolute_hit_path_as_is() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_source("notes", tmp.path());
        let h = hit("notes", "/absolute/note.md", "内容");
        assert_eq!(display_path(&cfg, &h), "/absolute/note.md");
    }

    #[test]
    fn display_path_falls_back_to_the_raw_path_for_an_unknown_source() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_source("notes", tmp.path());
        let h = hit("other-source", "note.md", "内容");
        assert_eq!(display_path(&cfg, &h), "note.md");
    }

    #[test]
    fn format_hits_lists_every_hit_with_score_and_ordinal() {
        let tmp = tempfile::tempdir().unwrap();
        let mut cfg = cfg_with_source("notes", tmp.path());
        cfg.embedding.base_url = Some("http://127.0.0.1:9".into());
        cfg.embedding.api_key = Some("k".into());
        cfg.embedding.model = Some("mock-model".into());
        let hits = vec![
            hit("notes", "a.md", "first body"),
            hit("notes", "b.md", "second body"),
        ];

        let out = format_hits(&cfg, &hits);

        assert!(out.contains("找到 2 条相关片段"), "{out}");
        assert!(out.contains("索引模型 mock-model"), "{out}");
        assert!(out.contains("[1] score=0.500"), "{out}");
        assert!(out.contains("[2] score=0.500"), "{out}");
        assert!(out.contains("片段 1"), "{out}");
        assert!(out.contains("first body"), "{out}");
        assert!(out.contains("second body"), "{out}");
        assert!(out.contains("read 工具"), "{out}");
    }

    #[test]
    fn format_hits_with_no_hits_reports_zero() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_source("notes", tmp.path());
        let out = format_hits(&cfg, &[]);
        assert!(out.contains("找到 0 条相关片段"), "{out}");
    }

    #[test]
    fn format_hits_truncates_a_long_hit_without_splitting_a_char() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = cfg_with_source("notes", tmp.path());
        let long = "中".repeat(MAX_HIT_CHARS + 100);
        let hits = vec![hit("notes", "a.md", &long)];

        let out = format_hits(&cfg, &hits);

        assert!(out.contains("…(片段过长已截断)"), "{out}");
        assert!(
            !out.contains(&long),
            "the full (over-long) body must not be printed"
        );
        // The kept head is exactly MAX_HIT_CHARS chars.
        let kept = "中".repeat(MAX_HIT_CHARS);
        assert!(out.contains(&kept), "the head is kept verbatim");
    }
}
