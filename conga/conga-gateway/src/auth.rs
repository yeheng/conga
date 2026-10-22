//! Gateway token authentication.
//!
//! The gateway drives a full agent (bash tool, file writes) — anyone who can
//! reach the port can run code as this user. Every `/ws` and `/api/*`
//! request must therefore present the gateway token:
//!
//! * `Authorization: Bearer <token>` header (REST clients), or
//! * `?token=<token>` query parameter (browser WebSocket, which cannot set
//!   headers on the upgrade request).
//!
//! The gateway serves no static assets; unmatched paths fall through to a
//! plain 404.
//!
//! Token resolution order:
//! 1. `CONGA_GATEWAY_TOKEN` env (set = use verbatim, no file is touched);
//! 2. `<config_dir>/gateway_token` (created on first start, mode 0600).
//!
//! The token value is never logged; only its source path is.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::state::AppState;

pub(crate) const TOKEN_FILE_NAME: &str = "gateway_token";

/// Where the effective token came from (for the startup log; never the value).
pub(crate) enum TokenSource {
    /// `CONGA_GATEWAY_TOKEN` was set; no file involved.
    Env,
    /// File existed and was read.
    File(PathBuf),
    /// File was generated (first start).
    Generated(PathBuf),
}

impl std::fmt::Display for TokenSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TokenSource::Env => write!(f, "CONGA_GATEWAY_TOKEN env"),
            TokenSource::File(p) | TokenSource::Generated(p) => write!(f, "{}", p.display()),
        }
    }
}

/// Resolve the token: env override > existing file > newly generated file.
pub(crate) fn load_or_create_token() -> Result<(String, TokenSource), String> {
    if let Ok(t) = std::env::var("CONGA_GATEWAY_TOKEN") {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Ok((t, TokenSource::Env));
        }
    }

    let path = conga::storage::config_dir().join(TOKEN_FILE_NAME);
    if let Ok(existing) = read_token_file(&path) {
        return Ok((existing, TokenSource::File(path)));
    }

    let token = generate_token();
    write_token_file(&path, &token).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok((token, TokenSource::Generated(path)))
}

/// 64 hex chars (two UUIDv4s = 244 random bits). Enough for a LAN personal
/// gateway; no new dependencies.
fn generate_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

fn read_token_file(path: &Path) -> std::io::Result<String> {
    let raw = std::fs::read_to_string(path)?;
    let t = raw.trim().to_string();
    if t.is_empty() {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "empty token file",
        ))
    } else {
        Ok(t)
    }
}

fn write_token_file(path: &Path, token: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(token.as_bytes())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, token)
    }
}

/// Constant-time comparison: no early exit on first differing byte.
fn token_matches(expected: &str, presented: &str) -> bool {
    let (a, b) = (expected.as_bytes(), presented.as_bytes());
    let mut diff: u8 = (a.len() ^ b.len()) as u8;
    for i in 0..a.len().min(b.len()) {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

fn presented_token(req: &Request) -> Option<String> {
    if let Some(auth) = req.headers().get(axum::http::header::AUTHORIZATION) {
        if let Ok(v) = auth.to_str() {
            if let Some(rest) = v
                .strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer "))
            {
                return Some(rest.trim().to_string());
            }
        }
    }
    // Browser WebSocket cannot set headers on the upgrade; accept ?token=.
    req.uri()
        .query()
        .and_then(|q| {
            q.split('&').find_map(|pair| {
                let (k, v) = pair.split_once('=')?;
                (k == "token").then(|| urldecode(v))
            })
        })
        .filter(|t| !t.is_empty())
}

/// Minimal percent-decoding for the `token` query value.
fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            // Need both hex digits (i+1, i+2) in bounds.
            b'%' if i + 3 <= bytes.len() => {
                if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(b);
                    i += 3;
                } else {
                    out.push(b'%');
                    i += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(
            axum::http::header::WWW_AUTHENTICATE,
            "Bearer realm=\"conga-gateway\"",
        )],
        Json(json!({ "error": "unauthorized: missing or invalid gateway token" })),
    )
        .into_response()
}

/// Axum middleware: gate `/ws` and `/api/*`; every other path has no route
/// and falls through to the router's default 404.
pub(crate) async fn require_token(
    State(state): State<Arc<AppState>>,
    req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path();
    if path != "/ws" && !path.starts_with("/api/") {
        return next.run(req).await;
    }
    match presented_token(&req) {
        Some(t) if token_matches(&state.auth_token, &t) => next.run(req).await,
        _ => unauthorized(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serialize env-dependent tests (std::env is process-global).
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn request(uri: &str, authorization: Option<&str>) -> Request {
        let mut b = Request::builder().uri(uri);
        if let Some(a) = authorization {
            b = b.header(axum::http::header::AUTHORIZATION, a);
        }
        b.body(axum::body::Body::empty()).unwrap()
    }

    fn test_state(token: &str) -> Arc<AppState> {
        Arc::new(AppState {
            sessions: dashmap::DashMap::new(),
            store_root: std::path::PathBuf::from("/tmp"),
            index_db: std::path::PathBuf::from("/tmp/index.db"),
            auth_token: Arc::new(token.to_string()),
        })
    }

    /// A router behind the token middleware.
    fn guarded_router(token: &str) -> axum::Router {
        let state = test_state(token);
        axum::Router::new()
            .route("/api/thing", axum::routing::get(|| async { "ok" }))
            .route("/ws", axum::routing::get(|| async { "ok" }))
            .route("/public", axum::routing::get(|| async { "ok" }))
            .layer(axum::middleware::from_fn_with_state(
                state.clone(),
                require_token,
            ))
            .with_state(state)
    }

    #[test]
    fn token_matches_equality_and_rejection() {
        assert!(token_matches("abc123", "abc123"));
        assert!(!token_matches("abc123", "abc124"));
        assert!(!token_matches("abc123", "abc1234"));
        assert!(!token_matches("abc123", ""));
        assert!(!token_matches("", "x"));
        assert!(token_matches("", ""));
    }

    #[test]
    fn token_source_display_names_the_env_or_the_file() {
        assert_eq!(TokenSource::Env.to_string(), "CONGA_GATEWAY_TOKEN env");
        let p = std::path::PathBuf::from("/tmp/gateway_token");
        assert_eq!(
            TokenSource::File(p.clone()).to_string(),
            "/tmp/gateway_token"
        );
        assert_eq!(TokenSource::Generated(p).to_string(), "/tmp/gateway_token");
    }

    #[test]
    fn presented_token_reads_the_bearer_header() {
        assert_eq!(
            presented_token(&request("/api/x", Some("Bearer abc"))).as_deref(),
            Some("abc")
        );
        // The scheme is matched case-insensitively.
        assert_eq!(
            presented_token(&request("/api/x", Some("bearer abc"))).as_deref(),
            Some("abc")
        );
        // Surrounding whitespace is trimmed.
        assert_eq!(
            presented_token(&request("/api/x", Some("Bearer  abc  "))).as_deref(),
            Some("abc")
        );
    }

    #[test]
    fn presented_token_falls_back_to_the_query_parameter() {
        assert_eq!(
            presented_token(&request("/ws?token=abc", None)).as_deref(),
            Some("abc")
        );
        // Percent- and plus-decoding.
        assert_eq!(
            presented_token(&request("/ws?token=a%20b", None)).as_deref(),
            Some("a b")
        );
        assert_eq!(
            presented_token(&request("/ws?token=a+b", None)).as_deref(),
            Some("a b")
        );
        // The first `token` parameter wins.
        assert_eq!(
            presented_token(&request("/ws?token=a&token=b", None)).as_deref(),
            Some("a")
        );
        // A header takes precedence over the query.
        assert_eq!(
            presented_token(&request("/ws?token=q", Some("Bearer h"))).as_deref(),
            Some("h")
        );
    }

    #[test]
    fn presented_token_is_none_without_a_usable_credential() {
        assert_eq!(presented_token(&request("/api/x", None)), None);
        // A foreign scheme is not a token.
        assert_eq!(presented_token(&request("/api/x", Some("Basic abc"))), None);
        // No space after the scheme: not the Bearer form.
        assert_eq!(presented_token(&request("/api/x", Some("Bearer"))), None);
        // An empty query value is rejected.
        assert_eq!(presented_token(&request("/ws?token=", None)), None);
        assert_eq!(presented_token(&request("/ws?other=1", None)), None);
    }

    #[tokio::test]
    async fn unauthorized_is_a_401_with_a_challenge() {
        let resp = unauthorized();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            resp.headers()
                .get(axum::http::header::WWW_AUTHENTICATE)
                .unwrap(),
            "Bearer realm=\"conga-gateway\""
        );
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"], "unauthorized: missing or invalid gateway token");
    }

    #[tokio::test]
    async fn require_token_gates_api_and_ws_paths_only() {
        use tower::util::ServiceExt;

        // No credential -> 401 on /api/*.
        let app = guarded_router("secret");
        let res = app
            .clone()
            .oneshot(request("/api/thing", None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        // A wrong token -> 401.
        let res = app
            .clone()
            .oneshot(request("/api/thing", Some("Bearer wrong")))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        // An empty bearer value -> 401 (never an empty-token match).
        let res = app
            .clone()
            .oneshot(request("/api/thing", Some("Bearer ")))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        // The right token -> through.
        let res = app
            .clone()
            .oneshot(request("/api/thing", Some("Bearer secret")))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // The query parameter works too (browser WebSocket).
        let res = app
            .clone()
            .oneshot(request("/api/thing?token=secret", None))
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);

        // /ws is gated as well.
        let res = app.clone().oneshot(request("/ws", None)).await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

        // A non-API path is not gated.
        let res = app.oneshot(request("/public", None)).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }

    #[test]
    fn file_roundtrip_stable_across_reads() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join(TOKEN_FILE_NAME);
        let t = generate_token();
        write_token_file(&p, &t).unwrap();

        let read = read_token_file(&p).unwrap();
        assert_eq!(read, t);

        // The token file must not be world/group readable on unix.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn empty_file_is_rejected_then_regenerated() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join(TOKEN_FILE_NAME);
        std::fs::write(&p, "   \n").unwrap();
        assert!(read_token_file(&p).is_err());
        let t = generate_token();
        write_token_file(&p, &t).unwrap();
        assert_eq!(read_token_file(&p).unwrap(), t);
    }

    #[test]
    fn env_override_wins_and_is_trimmed() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("CONGA_GATEWAY_TOKEN", "  env-token  \n");
        // config_dir() may point at the real ~/.conga — env must win before
        // any file access, and no file may be created by this path.
        let (t, src) = load_or_create_token().unwrap();
        assert_eq!(t, "env-token");
        assert!(matches!(src, TokenSource::Env));
        std::env::remove_var("CONGA_GATEWAY_TOKEN");
    }

    #[test]
    fn generated_tokens_are_long_and_unique() {
        let a = generate_token();
        let b = generate_token();
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn urldecode_handles_percent_and_plus() {
        assert_eq!(urldecode("abc%20def"), "abc def");
        assert_eq!(urldecode("a+b"), "a b");
        assert_eq!(urldecode("plain"), "plain");
        assert_eq!(urldecode("%zz"), "%zz"); // invalid hex passes through
    }
}
