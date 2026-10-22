//! `fetch` tool — HTTP GET a URL, convert HTML to readable markdown text.

use std::sync::Arc;
use std::time::Duration;

use conga::types::tool::{RiskLevel, ToolCallCtx, ToolDefinition, ToolResult};
use conga::ContentBlock;

/// Request timeout.
const TIMEOUT_SECS: u64 = 30;

pub fn tool() -> ToolDefinition {
    ToolDefinition {
        name: "fetch".into(),
        label: "Fetch".into(),
        description: "Fetch a URL and return its content as readable text. Converts HTML to markdown-like text (headings, links, lists). http/https only.".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "The http/https URL to fetch" }
            },
            "required": ["url"]
        }),
        risk: RiskLevel::Medium,
        execute: Arc::new(|ctx| Box::pin(execute(ctx))),
    }
}

async fn execute(ctx: ToolCallCtx) -> Result<ToolResult, conga::error::ToolError> {
    if ctx.aborted() {
        return Ok(ToolResult::error("aborted".to_string()));
    }
    let url = ctx.args["url"]
        .as_str()
        .ok_or_else(|| conga::error::ToolError::Message("url is required".into()))?;

    // Reject non-http(s) schemes early — defends against file:///etc/passwd etc.
    let scheme = url.split("://").next().unwrap_or("").to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Ok(ToolResult::error(format!(
            "unsupported URL scheme '{scheme}': only http/https allowed"
        )));
    }

    // SSRF guard: the model must not reach private/loopback/link-local
    // targets (cloud metadata endpoints, internal services) through us.
    // Opt out with CONGA_FETCH_ALLOW_PRIVATE_NET=1 (self-hosted LAN use).
    // For hostname URLs the guard also returns a DNS pin - every address it
    // validated - which is locked into the client below so reqwest can never
    // re-resolve the name (the classic DNS-rebinding TOCTOU).
    let pin = match ssrf_guard(url).await {
        Ok(pin) => pin,
        Err(msg) => return Ok(ToolResult::error(msg)),
    };

    let client = build_client(pin.as_ref())
        .map_err(|e| conga::error::ToolError::Message(format!("client build failed: {e}")))?;

    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| conga::error::ToolError::Message(format!("request failed: {e}")))?;

    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    let status = resp.status();
    if !status.is_success() {
        return Ok(ToolResult::error(format!("HTTP {status}")));
    }

    let body = resp
        .text()
        .await
        .map_err(|e| conga::error::ToolError::Message(format!("read body failed: {e}")))?;

    let text = if content_type.contains("html") {
        html_to_text(&body)
    } else {
        // Non-HTML: return raw text (JSON, plain text, etc.)
        body
    };

    let text = super::spill_or_truncate(&ctx, &text);
    Ok(ToolResult {
        content: vec![ContentBlock::text(text)],
        details: serde_json::json!({"url": url, "content_type": content_type}),
        is_error: false,
    })
}

/// A guard-validated DNS answer: the URL's host plus every address it
/// resolved to (all verified public). Pinned into the client via
/// `resolve_to_addrs`, closing the gap where a rebinding DNS server answers
/// the guard with a public IP and the actual request with 169.254.169.254.
struct SsrfPin {
    host: String,
    addrs: Vec<std::net::SocketAddr>,
}

/// Reject URLs whose host resolves (or literals point) into non-public
/// address space, returning the validated DNS pin for hostname URLs.
/// Skipped entirely when a tool proxy is configured - the proxy decides
/// where traffic may go - or when the operator opted out.
async fn ssrf_guard(url: &str) -> Result<Option<SsrfPin>, String> {
    if crate::proxy::tool_proxy().is_some() {
        return Ok(None);
    }
    if let Ok(v) = std::env::var("CONGA_FETCH_ALLOW_PRIVATE_NET") {
        if v == "1" || v.eq_ignore_ascii_case("true") {
            return Ok(None);
        }
    }
    let parsed = url::Url::parse(url).map_err(|e| format!("invalid URL: {e}"))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?;

    // Fast path: IP literal - check directly, no DNS, nothing to pin (the
    // connection can only go to the literal itself).
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return if is_non_public_ip(ip) {
            Err(format!(
                "refusing to fetch non-public address {host} (SSRF guard; \
                 set CONGA_FETCH_ALLOW_PRIVATE_NET=1 to allow)"
            ))
        } else {
            Ok(None)
        };
    }

    let port = parsed.port_or_known_default().unwrap_or(80);
    let addr = format!("{host}:{port}");

    // Hostname: resolve once and reject if ANY answer is non-public (a
    // public name that rebinds to an internal IP must not slip through).
    // Resolution failure fails CLOSED: without a validated address there is
    // nothing to pin, and proceeding unpinned would hand the destination
    // choice back to whatever a rebinding DNS answers next.
    let addrs: Vec<std::net::SocketAddr> =
        match tokio::time::timeout(Duration::from_secs(5), tokio::net::lookup_host(&addr)).await {
            Ok(Ok(addrs)) => addrs.collect(),
            Ok(Err(_)) | Err(_) => {
                return Err(format!(
                    "DNS resolution for {host} failed (SSRF guard requires a validated address)"
                ))
            }
        };
    if addrs.is_empty() {
        return Err(format!("host {host} resolved to no addresses (SSRF guard)"));
    }
    for a in &addrs {
        if is_non_public_ip(a.ip()) {
            return Err(format!(
                "host {host} resolves to non-public address {} (SSRF guard; \
                 set CONGA_FETCH_ALLOW_PRIVATE_NET=1 to allow)",
                a.ip()
            ));
        }
    }
    // Lowercased to match reqwest's override key normalization.
    Ok(Some(SsrfPin {
        host: host.to_ascii_lowercase(),
        addrs,
    }))
}

/// Build the fetch client. A DNS pin (guard-validated addresses only) is
/// bound via `resolve_to_addrs`, so the TCP connection is guaranteed to go
/// to an address the SSRF guard has seen - reqwest never re-resolves.
///
/// A pin and a proxy are mutually exclusive. reqwest honours `HTTP_PROXY`/
/// `HTTPS_PROXY`/`ALL_PROXY` on its own, and a proxied request is resolved by
/// the *proxy*, which would silently drop the pin and reopen the
/// DNS-rebinding window the guard just closed. The tool proxy
/// (`CONGA_TOOL_PROXY`) short-circuits the guard, so a pin and an explicit
/// tool proxy never coexist either.
fn build_client(pin: Option<&SsrfPin>) -> Result<reqwest::Client, reqwest::Error> {
    let builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .user_agent("conga-fetch/1.0");
    let builder = match pin {
        // Pinned: no proxy of any kind - the connection must land on the
        // guard-validated address.
        Some(pin) => builder
            .no_proxy()
            .resolve_to_addrs(&pin.host, &pin.addrs),
        // Unpinned: the tool proxy may be configured; reqwest's own system
        // proxy detection stays on.
        None => crate::proxy::apply_tool_proxy(builder),
    };
    builder.build()
}

/// Whether `ip` is anything other than a globally routable unicast address.
fn is_non_public_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            let o = v4.octets();
            o[0] == 0 // "this" network
                || o[0] == 10 // private
                || o[0] == 127 // loopback
                || (o[0] == 100 && o[1] & 0xC0 == 64) // CGNAT 100.64/10
                || (o[0] == 172 && o[1] & 0xF0 == 16) // private 172.16/12
                || (o[0] == 192 && o[1] == 168) // private
                || (o[0] == 169 && o[1] == 254) // link-local incl. cloud metadata
                || o[0] >= 224 // multicast + reserved + broadcast
        }
        std::net::IpAddr::V6(v6) => {
            // ::/128 (unspecified), ::1/128 (loopback), and every special
            // prefix; only global unicast 2000::/3 is public.
            v6.segments()[0] & 0xE000 != 0x2000
        }
    }
}

/// Convert HTML to readable plain text: strip non-content elements, extract
/// text from `<article>`/`<main>`/`<body>` in that priority.
fn html_to_text(html: &str) -> String {
    let doc = dom_query::Document::from(html);

    // Remove non-content noise elements.
    doc.select("script, style, nav, footer, header, noscript, iframe, svg")
        .remove();

    // Prefer semantic content containers; fall back to body.
    let content_sel = ["article", "main", "body"].into_iter().find_map(|sel| {
        let nodes = doc.select(sel);
        if nodes.iter().count() > 0 {
            Some(nodes)
        } else {
            None
        }
    });

    let raw = match content_sel {
        Some(nodes) => nodes
            .iter()
            .next()
            .map(|n| n.text().trim().to_string())
            .unwrap_or_default(),
        None => doc.select("body").text().trim().to_string(),
    };

    // Collapse excessive whitespace (HTML text often has lots of blank lines).
    raw.lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

// Char-safe truncation lives in `super::truncate_output` (shared with bash).

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_to_text_strips_script_and_style() {
        let html = r#"<html><body>
            <script>alert('xss')</script>
            <style>body { color: red; }</style>
            <p>Hello world</p>
        </body></html>"#;
        let text = html_to_text(html);
        assert!(text.contains("Hello world"));
        assert!(!text.contains("alert"));
        assert!(!text.contains("color"));
    }

    #[test]
    fn html_to_text_prefers_article() {
        let html = r#"<html><body>
            <nav>Menu</nav>
            <article><p>Main content here</p></article>
        </body></html>"#;
        let text = html_to_text(html);
        assert!(text.contains("Main content here"));
        assert!(!text.contains("Menu"));
    }

    #[test]
    fn html_to_text_falls_back_to_body() {
        let html = r#"<html><body><p>No article or main tag</p></body></html>"#;
        let text = html_to_text(html);
        assert!(text.contains("No article or main tag"));
    }

    /// Boundary: an empty document yields empty text (no panic).
    #[test]
    fn html_to_text_of_an_empty_document_is_empty() {
        assert_eq!(html_to_text(""), "");
    }

    /// Blank lines and indentation between blocks are collapsed away.
    #[test]
    fn html_to_text_collapses_blank_lines() {
        let html = "<html><body><p>one</p>\n\n   \n<p>two</p></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("one"), "{text:?}");
        assert!(text.contains("two"), "{text:?}");
        assert!(!text.contains("\n\n"), "blank lines must be collapsed: {text:?}");
    }

    #[test]
    fn truncate_adds_indicator() {
        let big = "x".repeat(crate::tools::MAX_OUTPUT_BYTES + 1000);
        let out = crate::tools::truncate_output(&big);
        assert!(out.len() < big.len());
    }

    #[test]
    fn truncate_noop_under_limit() {
        assert_eq!(crate::tools::truncate_output("small text"), "small text");
    }

    #[test]
    fn non_public_ip_classification() {
        let bad = [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "0.0.0.0",
            "100.64.0.1",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fe80::1",
            "fc00::1",
            "ff02::1",
        ];
        for s in bad {
            let ip: std::net::IpAddr = s.parse().unwrap();
            assert!(is_non_public_ip(ip), "{s} must be non-public");
        }
        let ok = [
            "1.1.1.1",
            "8.8.8.8",
            "172.32.0.1",
            "100.128.0.1",
            "2001:db8::1",
        ];
        for s in ok {
            let ip: std::net::IpAddr = s.parse().unwrap();
            assert!(!is_non_public_ip(ip), "{s} must be public");
        }
    }

    // The guard consults the global tool-proxy override; the mutex below
    // deliberately spans the await to serialize global-state tests (same
    // pattern as fetch_goes_through_tool_proxy).
    #[tokio::test]
    async fn fetch_rejects_cloud_metadata_endpoint() {
        // The guard consults the global tool-proxy override; hold the same
        // lock as the proxy test so a concurrent override can't skip it.
        let _g = crate::proxy::test_util::LOCK.lock().await;
        let ctx = ToolCallCtx {
            tool_call_id: "t3".into(),
            args: serde_json::json!({"url": "http://169.254.169.254/latest/meta-data/"}),
            signal: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            ctx: conga::ToolContext {
                cwd: ".".into(),
                env: std::collections::HashMap::new(),
                session_id: "t".into(),
                state_dir: ".".into(),
            },
        };
        let result = execute(ctx).await.unwrap();
        assert!(result.is_error);
        match &result.content[0] {
            ContentBlock::Text { text } => assert!(text.contains("SSRF guard"), "got: {text}"),
            _ => panic!("expected text content"),
        }
    }
    #[tokio::test]
    async fn fetch_rejects_localhost_by_resolution() {
        // No proxy configured and no opt-out: `localhost` resolves to a
        // loopback IP and must be refused before any request is made.
        let _g = crate::proxy::test_util::LOCK.lock().await;
        let ctx = ToolCallCtx {
            tool_call_id: "t4".into(),
            args: serde_json::json!({"url": "http://localhost:9/"}),
            signal: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            ctx: conga::ToolContext {
                cwd: ".".into(),
                env: std::collections::HashMap::new(),
                session_id: "t".into(),
                state_dir: ".".into(),
            },
        };
        let result = execute(ctx).await.unwrap();
        assert!(result.is_error, "localhost must be blocked: {result:?}");
    }

    #[tokio::test]
    async fn fetch_rejects_unresolvable_host_closed() {
        // With pinning, a hostname the guard cannot resolve must fail CLOSED:
        // proceeding unpinned would hand destination choice back to DNS.
        let _g = crate::proxy::test_util::LOCK.lock().await;
        let ctx = ToolCallCtx {
            tool_call_id: "t5".into(),
            args: serde_json::json!({"url": "http://definitely-not-a-real-host.invalid/"}),
            signal: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            ctx: conga::ToolContext {
                cwd: ".".into(),
                env: std::collections::HashMap::new(),
                session_id: "t".into(),
                state_dir: ".".into(),
            },
        };
        let result = execute(ctx).await.unwrap();
        assert!(
            result.is_error,
            "unresolvable host must never yield a successful fetch"
        );
        // On a normal resolver the guard itself refuses (fail-closed: no
        // validated address, no pin, no request). On wildcard-DNS networks
        // the request still fails - either way nothing is fetched.
    }

    /// The pin is the SSRF guard's guarantee that the request lands on an
    /// address the guard validated. A proxy resolves the name on our behalf,
    /// so a proxied client silently drops that guarantee — reqwest picks up
    /// `HTTP_PROXY`/`HTTPS_PROXY`/`ALL_PROXY` on its own, without the guard
    /// or the tool-proxy override ever seeing them.
    ///
    /// reqwest's `Client` debug output lists `proxies` only when the client
    /// actually carries one, which makes this observable without touching
    /// process-wide environment variables.
    #[test]
    fn pinned_client_carries_no_proxy() {
        let pin = SsrfPin {
            host: "pinned.test".into(),
            addrs: vec!["127.0.0.1:8080".parse().unwrap()],
        };
        let client = build_client(Some(&pin)).unwrap();
        let dbg = format!("{client:?}");
        assert!(
            !dbg.contains("proxies"),
            "a pinned client must not use any proxy (system or tool): {dbg}"
        );
    }

    /// The anti-rebinding core: a client built with a DNS pin must connect
    /// to the PINNED address, never re-resolve the name. A local listener
    /// plays the pinned target; `rebind.test` has no real DNS record, so the
    /// request can only arrive if the pin (not DNS) decides the destination.
    /// Without pinning this fails with a DNS error, not a served response.
    #[tokio::test]
    async fn pinned_client_connects_to_pinned_address_not_dns() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // build_client consults the global tool-proxy override; hold the
        // same lock as the proxy tests so a concurrent override can't
        // reroute this client.
        let _g = crate::proxy::test_util::LOCK.lock().await;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            loop {
                let n = sock.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
            let body = "pinned-content";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            buf
        });

        // In production the pin only ever carries guard-validated public
        // addresses; here a loopback addr stands in as the pinned target to
        // prove the connection layer honors the pin verbatim.
        let pin = SsrfPin {
            host: "rebind.test".into(),
            addrs: vec![addr],
        };
        let client = build_client(Some(&pin)).unwrap();
        let resp = client.get("http://rebind.test/").send().await.unwrap();
        assert!(resp.status().is_success());
        assert_eq!(resp.text().await.unwrap(), "pinned-content");

        let head = server.await.unwrap();
        let head = String::from_utf8_lossy(&head);
        assert!(head.starts_with("GET /"), "pinned server saw: {head}");
    }
    #[tokio::test]
    async fn fetch_rejects_non_http_scheme() {
        let ctx = ToolCallCtx {
            tool_call_id: "t1".into(),
            args: serde_json::json!({"url": "file:///etc/passwd"}),
            signal: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            ctx: conga::ToolContext {
                cwd: ".".into(),
                env: std::collections::HashMap::new(),
                session_id: "t".into(),
                state_dir: ".".into(),
            },
        };
        let result = execute(ctx).await.unwrap();
        assert!(result.is_error);
        match &result.content[0] {
            ContentBlock::Text { text } => {
                assert!(text.contains("unsupported URL scheme"));
            }
            _ => panic!("expected text content"),
        }
    }

    /// End-to-end wiring proof: with the override set, fetch's request must
    /// hit the proxy, not the target host. A real HTTP proxy in ~25 lines:
    /// read the request head, reply with a canned page.
    #[tokio::test]
    async fn fetch_goes_through_tool_proxy() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let _g = crate::proxy::test_util::LOCK.lock().await;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut head = String::new();
            let mut buf = [0u8; 8192];
            loop {
                let n = sock.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                head.push_str(&String::from_utf8_lossy(&buf[..n]));
                if head.contains("\r\n\r\n") {
                    break;
                }
            }
            let body = "<html><body><article>via proxy</article></body></html>";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
            head
        });

        crate::proxy::set_tool_proxy(Some(&format!("http://{proxy_addr}"))).unwrap();
        let ctx = ToolCallCtx {
            tool_call_id: "t2".into(),
            args: serde_json::json!({"url": "http://example.test/"}),
            signal: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            ctx: conga::ToolContext {
                cwd: ".".into(),
                env: std::collections::HashMap::new(),
                session_id: "t".into(),
                state_dir: ".".into(),
            },
        };
        let result = execute(ctx).await.unwrap();
        crate::proxy::set_tool_proxy(None).unwrap();

        assert!(!result.is_error);
        match &result.content[0] {
            ContentBlock::Text { text } => assert!(text.contains("via proxy")),
            _ => panic!("expected text content"),
        }
        // A proxied http request carries the absolute target URI on the
        // request line — proof the connection went through the proxy.
        let head = server.await.unwrap();
        assert!(
            head.starts_with("GET http://example.test/"),
            "proxy saw: {head}"
        );
    }

    // ---- tool definition ----

    #[test]
    fn tool_definition_exposes_a_url_parameter() {
        let t = tool();
        assert_eq!(t.name, "fetch");
        assert_eq!(t.label, "Fetch");
        assert_eq!(t.risk, conga::types::tool::RiskLevel::Medium);
        assert!(
            t.parameters["properties"]["url"]["type"] == "string",
            "{:?}",
            t.parameters
        );
        assert_eq!(t.parameters["required"], serde_json::json!(["url"]));
        assert!(
            t.description.contains("http/https only"),
            "{}",
            t.description
        );
    }

    // ---- ssrf guard ----
    //
    // Every case below uses an IP LITERAL (or a malformed URL), so the
    // guard never performs a DNS lookup.

    /// `SsrfPin` has no `Debug` impl, so `unwrap_err` is unavailable here —
    /// unwrap the error by hand.
    async fn guard_err(url: &str) -> String {
        match ssrf_guard(url).await {
            Ok(_) => panic!("expected {url} to be refused by the SSRF guard"),
            Err(e) => e,
        }
    }

    /// Happy path: a public IPv4 literal is allowed, and needs no DNS pin
    /// (the connection can only go to the literal itself).
    #[tokio::test]
    async fn ssrf_guard_allows_public_ip_literal() {
        let _g = crate::proxy::test_util::LOCK.lock().await;
        let pin = ssrf_guard("http://1.1.1.1/").await.expect("public literal");
        assert!(pin.is_none(), "an IP literal needs no DNS pin");
    }

    #[tokio::test]
    async fn ssrf_guard_rejects_loopback_ip_literal() {
        let _g = crate::proxy::test_util::LOCK.lock().await;
        let err = guard_err("http://127.0.0.1/").await;
        assert!(err.contains("non-public address"), "{err}");
        assert!(err.contains("127.0.0.1"), "{err}");
        assert!(err.contains("SSRF guard"), "{err}");
    }

    #[tokio::test]
    async fn ssrf_guard_rejects_private_network_ip_literals() {
        let _g = crate::proxy::test_util::LOCK.lock().await;
        for host in ["10.0.0.1", "172.16.0.1", "192.168.1.1"] {
            let err = guard_err(&format!("http://{host}/")).await;
            assert!(err.contains("non-public address"), "{host}: {err}");
            assert!(err.contains(host), "{host}: {err}");
        }
    }

    /// The cloud metadata endpoint is the classic SSRF target.
    #[tokio::test]
    async fn ssrf_guard_rejects_cloud_metadata_ip_literal() {
        let _g = crate::proxy::test_util::LOCK.lock().await;
        let err = guard_err("http://169.254.169.254/latest/meta-data/").await;
        assert!(err.contains("169.254.169.254"), "{err}");
        assert!(err.contains("SSRF guard"), "{err}");
        assert!(
            err.contains("CONGA_FETCH_ALLOW_PRIVATE_NET=1"),
            "the opt-out must be named: {err}"
        );
    }

    /// IPv6 literals are refused too. (`Url::host_str` returns the bracketed
    /// form, so the guard reports them via the resolution branch — the
    /// wording differs from the IPv4 fast path, the verdict does not.)
    #[tokio::test]
    async fn ssrf_guard_rejects_ipv6_loopback_literal() {
        let _g = crate::proxy::test_util::LOCK.lock().await;
        let err = guard_err("http://[::1]/").await;
        assert!(err.contains("non-public address"), "{err}");
        assert!(err.contains("::1"), "{err}");
    }

    #[tokio::test]
    async fn ssrf_guard_rejects_ipv6_link_local_literal() {
        let _g = crate::proxy::test_util::LOCK.lock().await;
        let err = guard_err("http://[fe80::1]/").await;
        assert!(err.contains("non-public address"), "{err}");
        assert!(err.contains("fe80::1"), "{err}");
    }

    /// Happy path: a public IPv6 literal is allowed.
    #[tokio::test]
    async fn ssrf_guard_allows_public_ipv6_literal() {
        let _g = crate::proxy::test_util::LOCK.lock().await;
        assert!(
            ssrf_guard("http://[2001:db8::1]/").await.is_ok(),
            "a global-unicast IPv6 literal must be allowed"
        );
    }

    /// Error path: an unparseable URL never reaches the network.
    #[tokio::test]
    async fn ssrf_guard_rejects_unparseable_url() {
        let _g = crate::proxy::test_util::LOCK.lock().await;
        let err = guard_err("not-a-url").await;
        assert!(err.contains("invalid URL"), "{err}");
    }

    /// Error path: a URL without a host (e.g. `file:///etc/passwd`) has
    /// nothing to validate.
    #[tokio::test]
    async fn ssrf_guard_rejects_url_without_host() {
        let _g = crate::proxy::test_util::LOCK.lock().await;
        let err = guard_err("file:///etc/passwd").await;
        assert!(err.contains("no host"), "{err}");
    }

    /// When a tool proxy is configured the proxy decides where traffic may
    /// go, so the guard steps aside entirely (no pin, no rejection).
    #[tokio::test]
    async fn ssrf_guard_is_skipped_when_a_tool_proxy_is_configured() {
        let _g = crate::proxy::test_util::LOCK.lock().await;
        crate::proxy::set_tool_proxy(Some("http://127.0.0.1:1")).unwrap();
        let res = ssrf_guard("http://127.0.0.1/").await;
        // Reset before asserting: a failing assert must not leave the
        // global override installed for other tests.
        crate::proxy::set_tool_proxy(None).unwrap();
        assert!(res.is_ok(), "the guard must stand down for a proxied request");
        assert!(res.unwrap().is_none());
    }
}
