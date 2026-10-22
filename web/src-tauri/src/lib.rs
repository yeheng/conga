//! Conga desktop backend.
//!
//! The session-management API lives here as Tauri commands so the desktop
//! app is self-contained: it reads/writes the on-disk session store
//! (`~/.conga/sessions`) directly through conga/conga-host instead of
//! depending on a separately running gateway process. The `chat` module
//! goes one step further and hosts the agent loop itself: per-session Hosts
//! stream turn events over Tauri IPC (`chat-event`), replacing the gateway's
//! WebSocket transport inside the desktop shell. The gateway remains the
//! transport for plain-browser (dev) usage.
//!
//! Session commands are thin wrappers over `conga_host::session_api` — the
//! SAME implementations the gateway's REST handlers call. One validation
//! rule, one DTO shape, one fail-loud policy.

use conga_host::session_api::{self, SessionListItem};

mod chat;

/// Serializes the tests that move `$HOME` — it is process-global state, so
/// two tests that relocate the app config root must not overlap.
#[cfg(test)]
pub(crate) static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn session_store_root() -> std::path::PathBuf {
    conga::JsonlStorage::default_root().base_dir_clone()
}

#[tauri::command]
async fn list_sessions() -> Result<Vec<SessionListItem>, String> {
    session_api::list_sessions(&session_store_root())
        .await
        .map_err(|e| e.to_string())
}

/// Backend-truth transcript for a session. `Ok(None)` means the session has
/// no on-disk data yet (a local-only chat) — the frontend keeps its local
/// state in that case. Corruption fails loud with `Err`.
#[tauri::command]
async fn get_session_messages(id: String) -> Result<Option<Vec<serde_json::Value>>, String> {
    session_api::session_messages(&session_store_root(), &id)
        .await
        .map_err(|e| e.to_string())
        .and_then(|messages| {
            messages
                .map(|m| {
                    serde_json::to_value(m)
                        .map(|v| v.as_array().cloned().unwrap_or_default())
                        .map_err(|e| e.to_string())
                })
                .transpose()
        })
}

/// Persist the session's display name (meta.json sidecar). Creates the
/// session directory if needed, so a chat can be named before its first
/// turn lands on disk. Validation is shared with the gateway.
#[tauri::command]
async fn rename_session(id: String, name: String) -> Result<(), String> {
    session_api::rename_session(&session_store_root(), &id, &name)
        .await
        .map_err(|e| e.to_string())
}

/// Cross-session full-text search (FTS5 sidecar at `~/.conga/index.db`).
/// Same engine as the gateway's REST route (conga_host::session_api):
/// incremental high-water reindex check + query, awaited directly.
#[tauri::command]
async fn search_sessions(
    query: String,
) -> Result<Vec<conga_host::session_index::SessionHit>, String> {
    let root = session_store_root();
    let db = conga::storage::config_dir().join("index.db");
    session_api::search_sessions(&root, &db, &query, 20)
        .await
        .map_err(|e| e.to_string())
}

/// Delete the session's on-disk data wholesale (event log + meta sidecar).
#[tauri::command]
async fn delete_session(id: String) -> Result<bool, String> {
    session_api::delete_session(&session_store_root(), &id)
        .await
        .map_err(|e| e.to_string())
}

/// `~/.conga/app_config.json` — the desktop shell's durable mirror of the
/// browser build's localStorage preferences (theme, sidebar state, chats
/// meta, hidden sessions). One JSON object keyed by storage key; values are
/// parsed JSON when possible, else the raw string — the frontend round-trips
/// them back into localStorage byte-for-byte. Same fail-loud conventions as
/// the session store: corruption is an error, never silently re-created.
fn app_config_path() -> std::path::PathBuf {
    conga::storage::config_dir().join("app_config.json")
}

/// Extract `conga_proxy` from the app config and install it as the
/// fetch/web_search proxy override. Missing or empty clears the override
/// (direct connection). Values may be raw strings (writeString path — not
/// JSON) or JSON strings; `as_str` covers both.
fn apply_proxy_from_config(config: &serde_json::Value) -> Result<(), String> {
    let url = config
        .get("conga_proxy")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    conga_host::set_tool_proxy(url).map_err(|e| format!("conga_proxy invalid: {e}"))
}

#[tauri::command]
fn get_app_config() -> Result<Option<serde_json::Value>, String> {
    match std::fs::read(app_config_path()) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| format!("app config corrupt: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

/// Atomic write (tmp + rename): a crash can never leave a torn config
/// shadowing an intact one. The file is tiny and writes are debounced by the
/// frontend, so a blocking std::fs write is noise.
#[tauri::command]
fn set_app_config(config: serde_json::Value) -> Result<(), String> {
    let path = app_config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let bytes = serde_json::to_vec_pretty(&config).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    // Apply only after the write succeeded, so runtime state can never
    // diverge from what is persisted.
    apply_proxy_from_config(&config)
}

/// Check a proxy URL against the same validation `set_tool_proxy` uses,
/// without installing it. The dialog calls this before saving so a bad
/// URL fails in the UI, not in a console.warn.
#[tauri::command]
fn validate_proxy(url: String) -> Result<(), String> {
    let url = url.trim();
    if url.is_empty() {
        return Ok(()); // clearing is always valid
    }
    conga_host::validate_tool_proxy(url)
}

/// The masked LLM env settings (raw keys never leave the process). See
#[tauri::command]
fn get_env_settings() -> serde_json::Value {
    conga_host::settings::settings_to_masked_json(&conga_host::settings::load_settings())
}

/// Validate → merge (blank `apiKey` keeps the stored one) → persist
/// atomically. The in-process Host re-resolves its provider from this file
/// every turn, so the next LLM call uses the new settings.
#[tauri::command]
fn set_env_settings(payload: serde_json::Value) -> Result<serde_json::Value, String> {
    conga_host::settings::put_settings(&payload)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let _ = dotenvy::dotenv();
    // conga/conga-host emit through `tracing`; without a global
    // subscriber those records vanish. This is separate from tauri-plugin-log
    // (fern, registered in setup) which handles `log`-crate records — the two
    // coexist only because tracing-subscriber is built without `tracing-log`
    // (see Cargo.toml). Bare `EnvFilter::from_default_env()` defaults to
    // ERROR-only when RUST_LOG is unset, so fall back to `info`.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .manage(chat::ChatState::new())
        .invoke_handler(tauri::generate_handler![
            list_sessions,
            get_session_messages,
            rename_session,
            search_sessions,
            delete_session,
            chat::send_message,
            chat::cancel_turn,
            chat::approval_response,
            chat::get_context,
            get_app_config,
            set_app_config,
            validate_proxy,
            get_env_settings,
            set_env_settings,
        ])
        .setup(|_app| {
            if let Ok(Some(config)) = get_app_config() {
                if let Err(e) = apply_proxy_from_config(&config) {
                    log::warn!("skipping invalid stored proxy: {e}");
                }
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;

    use conga::{AgentMessage, ContentBlock, EventStorage, SessionEvent, UserMessage};

    struct NoopLogger;

    impl log::Log for NoopLogger {
        fn enabled(&self, _: &log::Metadata) -> bool {
            false
        }
        fn log(&self, _: &log::Record) {}
        fn flush(&self) {}
    }

    /// The tracing subscriber init must NOT claim the global `log` logger —
    /// tauri-plugin-log (fern) needs it, and losing that race aborts the app
    /// during setup. Guards the `default-features = false` on
    /// tracing-subscriber in Cargo.toml: re-enabling `tracing-log` makes
    /// `fmt().init()` install LogTracer and fails this test.
    #[test]
    fn tracing_init_leaves_log_logger_free() {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .init();
        assert!(log::set_boxed_logger(Box::new(NoopLogger)).is_ok());
    }

    // ------------------------------------------------------------------
    // App config root isolation
    //
    // The commands resolve their storage through `conga::storage::config_dir()`,
    // i.e. `dirs::home_dir()`. That helper re-reads `$HOME` on every call and
    // never caches it, so moving `$HOME` into a temp dir moves the whole
    // `~/.conga` root with it — no production seam required.
    // ------------------------------------------------------------------

    /// Serializes the tests that move `$HOME`: it is process-global state.
    struct TempHome {
        dir: tempfile::TempDir,
        _guard: std::sync::MutexGuard<'static, ()>,
        prev_home: Option<std::ffi::OsString>,
    }

    impl TempHome {
        fn new() -> Self {
            let guard = crate::HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let prev_home = std::env::var_os("HOME");
            let dir = tempfile::tempdir().expect("temp home");
            std::env::set_var("HOME", dir.path());
            Self {
                dir,
                _guard: guard,
                prev_home,
            }
        }

        /// `<home>/.conga`
        fn conga_dir(&self) -> std::path::PathBuf {
            self.dir.path().join(".conga")
        }

        /// `<home>/.conga/sessions`
        fn sessions_dir(&self) -> std::path::PathBuf {
            self.conga_dir().join("sessions")
        }
    }

    impl Drop for TempHome {
        fn drop(&mut self) {
            match self.prev_home.take() {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    /// The smallest possible session log entry.
    fn user_event(text: &str) -> SessionEvent {
        SessionEvent::User(AgentMessage::User(UserMessage {
            content: vec![ContentBlock::text(text)],
            timestamp: 1,
        }))
    }

    /// Write a one-message session into the (redirected) store.
    async fn seed_session(id: &str, text: &str) {
        EventStorage::new(session_store_root())
            .append_event(id, &user_event(text))
            .await
            .expect("seed session");
    }

    /// Run `f`, then restore the process-global tool-proxy override. The
    /// override lives in conga-host's process globals, so a test that
    /// installs one must not leak it into the next test.
    fn with_proxy_restored(f: impl FnOnce()) {
        let baseline = conga_host::tool_proxy();
        f();
        let _ = conga_host::set_tool_proxy(baseline.as_deref());
    }

    #[tokio::test]
    async fn session_store_root_lives_under_the_conga_home() {
        let home = TempHome::new();
        assert_eq!(session_store_root(), home.sessions_dir());
    }

    #[tokio::test]
    async fn list_sessions_is_empty_on_a_fresh_install() {
        let _home = TempHome::new();
        assert!(list_sessions().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn list_sessions_reports_seeded_sessions() {
        let _home = TempHome::new();
        seed_session("sess-1", "hello").await;
        let items = list_sessions().await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "sess-1");
        assert_eq!(items[0].msg_count, 1);
        assert!(items[0].name.is_none());
    }

    #[tokio::test]
    async fn get_session_messages_is_none_for_an_unknown_session() {
        let _home = TempHome::new();
        assert!(get_session_messages("ghost".into()).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn get_session_messages_returns_the_transcript() {
        let _home = TempHome::new();
        seed_session("sess-1", "hello").await;
        let msgs = get_session_messages("sess-1".into())
            .await
            .unwrap()
            .expect("session has data");
        assert_eq!(msgs.len(), 1);
        let raw = serde_json::to_string(&msgs[0]).unwrap();
        assert!(raw.contains("hello"), "transcript lost the message: {raw}");
    }

    #[tokio::test]
    async fn rename_session_persists_the_display_name() {
        let _home = TempHome::new();
        rename_session("sess-1".into(), "  Release notes  ".into())
            .await
            .unwrap();
        let items = list_sessions().await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name.as_deref(), Some("Release notes"));
    }

    #[tokio::test]
    async fn rename_session_rejects_invalid_ids_and_names() {
        let _home = TempHome::new();
        // Path traversal and empty ids must never reach the filesystem.
        assert!(rename_session("../evil".into(), "ok".into()).await.is_err());
        assert!(rename_session("".into(), "ok".into()).await.is_err());
        assert!(rename_session("sess/1".into(), "ok".into()).await.is_err());
        // A blank name is not a name.
        assert!(rename_session("sess-1".into(), "   ".into()).await.is_err());
        // 200 chars is the documented maximum; 201 is one over.
        assert!(rename_session("sess-1".into(), "x".repeat(200).into())
            .await
            .is_ok());
        assert!(rename_session("sess-1".into(), "x".repeat(201).into())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn delete_session_removes_the_session_exactly_once() {
        let _home = TempHome::new();
        seed_session("sess-1", "hello").await;
        assert!(delete_session("sess-1".into()).await.unwrap());
        assert!(!delete_session("sess-1".into()).await.unwrap());
        assert!(list_sessions().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn delete_session_rejects_invalid_ids() {
        let _home = TempHome::new();
        assert!(delete_session("../evil".into()).await.is_err());
        assert!(delete_session("".into()).await.is_err());
    }

    #[tokio::test]
    async fn search_sessions_rejects_a_blank_query() {
        let _home = TempHome::new();
        assert!(search_sessions("   ".into()).await.is_err());
    }

    #[tokio::test]
    async fn search_sessions_finds_a_seeded_session() {
        let _home = TempHome::new();
        seed_session("sess-1", "hello").await;
        let hits = search_sessions("hello".into()).await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_id, "sess-1");
    }

    #[test]
    fn app_config_path_lives_under_the_conga_home() {
        let home = TempHome::new();
        assert_eq!(
            app_config_path(),
            home.conga_dir().join("app_config.json")
        );
    }

    #[test]
    fn get_app_config_is_none_when_absent() {
        let _home = TempHome::new();
        assert!(get_app_config().unwrap().is_none());
    }

    #[test]
    fn set_app_config_round_trips_through_the_file() {
        let _home = TempHome::new();
        with_proxy_restored(|| {
            let cfg = serde_json::json!({ "theme": "dark", "conga_proxy": "" });
            set_app_config(cfg.clone()).unwrap();
            assert_eq!(get_app_config().unwrap().unwrap(), cfg);
        });
    }

    #[test]
    fn get_app_config_fails_loud_on_a_corrupt_file() {
        let home = TempHome::new();
        std::fs::create_dir_all(home.conga_dir()).unwrap();
        std::fs::write(
            home.conga_dir().join("app_config.json"),
            b"{ this is not json",
        )
        .unwrap();
        let err = get_app_config().unwrap_err();
        assert!(err.contains("corrupt"), "got: {err}");
    }

    #[test]
    fn apply_proxy_from_config_installs_clears_and_rejects() {
        let _home = TempHome::new();
        let baseline = conga_host::tool_proxy();
        with_proxy_restored(|| {
            // A missing key clears the override (direct connection).
            apply_proxy_from_config(&serde_json::json!({})).unwrap();
            assert_eq!(conga_host::tool_proxy(), baseline);

            // A valid URL is installed verbatim.
            apply_proxy_from_config(&serde_json::json!({ "conga_proxy": "http://127.0.0.1:9" }))
                .unwrap();
            assert_eq!(
                conga_host::tool_proxy().as_deref(),
                Some("http://127.0.0.1:9")
            );

            // A blank value clears it again.
            apply_proxy_from_config(&serde_json::json!({ "conga_proxy": "   " })).unwrap();
            assert_eq!(conga_host::tool_proxy(), baseline);

            // An unsupported scheme is rejected, and the override is left
            // untouched by the failure.
            let err = apply_proxy_from_config(&serde_json::json!({ "conga_proxy": "ftp://x.test" }))
                .unwrap_err();
            assert!(err.contains("conga_proxy invalid"), "got: {err}");
            assert_eq!(conga_host::tool_proxy(), baseline);
        });
    }

    #[test]
    fn validate_proxy_accepts_clearing_and_known_schemes() {
        assert!(validate_proxy("".into()).is_ok());
        assert!(validate_proxy("   ".into()).is_ok());
        assert!(validate_proxy("http://127.0.0.1:8080".into()).is_ok());
        assert!(validate_proxy("https://proxy.test".into()).is_ok());
        assert!(validate_proxy("socks5://127.0.0.1:1080".into()).is_ok());
        assert!(validate_proxy("socks5h://127.0.0.1:1080".into()).is_ok());
    }

    #[test]
    fn validate_proxy_rejects_unsupported_schemes() {
        let err = validate_proxy("ftp://proxy.test".into()).unwrap_err();
        assert!(err.contains("unsupported proxy scheme"), "got: {err}");
    }

    #[test]
    fn set_app_config_persists_the_file_even_when_the_proxy_is_invalid() {
        // Documented current behaviour: the file write happens first, so an
        // invalid proxy is reported to the caller while the (unusable) value
        // stays on disk. Startup re-reads the file and skips the invalid
        // proxy with a warning.
        let home = TempHome::new();
        with_proxy_restored(|| {
            let err = set_app_config(serde_json::json!({ "conga_proxy": "ftp://x.test" }))
                .unwrap_err();
            assert!(err.contains("conga_proxy invalid"), "got: {err}");
            let saved = get_app_config().unwrap().unwrap();
            assert_eq!(saved["conga_proxy"], serde_json::json!("ftp://x.test"));
            assert!(home.conga_dir().join("app_config.json").exists());
        });
    }

    #[test]
    fn get_env_settings_never_leaks_the_api_key() {
        let home = TempHome::new();
        std::fs::create_dir_all(home.conga_dir()).unwrap();
        std::fs::write(
            home.conga_dir().join("settings.json"),
            serde_json::json!({
                "llm": {
                    "baseUrl": "https://api.test/v1",
                    "model": "test-model",
                    "apiKey": "sk-secret-value-123456",
                    "api": "openai"
                }
            })
            .to_string(),
        )
        .unwrap();

        let masked = get_env_settings();
        let raw = serde_json::to_string(&masked).unwrap();
        assert!(!raw.contains("sk-secret-value-123456"), "leaked key: {raw}");
        assert_eq!(masked["llm"]["apiKeySet"], serde_json::json!(true));
        assert_eq!(masked["llm"]["model"], serde_json::json!("test-model"));
        assert_eq!(masked["llm"]["api"], serde_json::json!("openai"));
    }

    #[test]
    fn get_env_settings_is_empty_without_a_settings_file() {
        let _home = TempHome::new();
        let masked = get_env_settings();
        assert_eq!(masked["llm"], serde_json::Value::Null);
        assert_eq!(masked["fastLlm"], serde_json::Value::Null);
    }

    #[test]
    fn set_env_settings_persists_and_returns_the_masked_view() {
        let home = TempHome::new();
        let out = set_env_settings(serde_json::json!({
            "llm": {
                "baseUrl": "https://api.test/v1",
                "model": "test-model",
                "apiKey": "sk-abcdef123456",
                "api": "openai"
            }
        }))
        .unwrap();

        let raw = serde_json::to_string(&out).unwrap();
        assert!(!raw.contains("sk-abcdef123456"), "leaked key: {raw}");
        assert_eq!(out["llm"]["apiKeySet"], serde_json::json!(true));

        // The raw key IS persisted on disk — the mask only guards the wire.
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(home.conga_dir().join("settings.json")).unwrap())
                .unwrap();
        assert_eq!(saved["llm"]["apiKey"], serde_json::json!("sk-abcdef123456"));
    }

    #[test]
    fn set_env_settings_rejects_an_invalid_group() {
        let _home = TempHome::new();
        // A key is present, so the group reaches field validation.
        let err = set_env_settings(serde_json::json!({
            "llm": { "baseUrl": "not-a-url", "model": "test-model", "apiKey": "sk-abcdef123456" }
        }))
        .unwrap_err();
        assert!(err.starts_with("llm:"), "got: {err}");
        assert!(err.contains("base_url"), "got: {err}");

        // An unknown api value is rejected too.
        let err = set_env_settings(serde_json::json!({
            "llm": {
                "baseUrl": "https://api.test/v1",
                "model": "test-model",
                "apiKey": "sk-abcdef123456",
                "api": "gemini"
            }
        }))
        .unwrap_err();
        assert!(err.contains("api must be openai or anthropic"), "got: {err}");
    }

    #[test]
    fn set_env_settings_requires_a_key_when_none_is_stored() {
        let _home = TempHome::new();
        // No stored key to inherit and none supplied -> rejected.
        let err = set_env_settings(serde_json::json!({
            "llm": { "baseUrl": "https://api.test/v1", "model": "test-model", "apiKey": "" }
        }))
        .unwrap_err();
        assert!(err.contains("apiKey is required"), "got: {err}");
    }
}
