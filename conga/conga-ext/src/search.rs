//! Web search tool with multiple providers.
//!
//! Environment variables:
//! - CONGA_SEARCH_PROVIDER: "duckduckgo" (default, no key required), "serper",
//!   "serpapi", "brave", "tavily", "exa", or "firecrawl"
//! - CONGA_SERPER_API_KEY: Serper.dev API key
//! - CONGA_SERPAPI_API_KEY: SerpAPI API key
//! - CONGA_BRAVE_API_KEY: Brave Search API key
//! - CONGA_TAVILY_API_KEY: Tavily API key
//! - CONGA_EXA_API_KEY: Exa API key
//! - CONGA_FIRECRAWL_API_KEY: Firecrawl API key
//! - CONGA_TOOL_PROXY: optional proxy (http/https/socks5/socks5h) for search traffic

use std::sync::Arc;

use conga::{ContentBlock, ExtensionApi, RiskLevel, ToolDefinition, ToolError, ToolResult};
use reqwest::Client;
use serde::Deserialize;
use tracing::{info, warn};

// ── Search result abstraction ────────────────────────────────────────────

struct SearchHit {
    title: String,
    snippet: String,
    url: String,
}

const MAX_SNIPPET_LEN: usize = 300;

fn format_hits(hits: &[SearchHit]) -> String {
    if hits.is_empty() {
        return "No results found.".to_string();
    }
    let mut out = String::new();
    for (i, h) in hits.iter().enumerate() {
        let snippet = if h.snippet.chars().count() > MAX_SNIPPET_LEN {
            format!(
                "{}...",
                h.snippet.chars().take(MAX_SNIPPET_LEN).collect::<String>()
            )
        } else {
            h.snippet.clone()
        };
        out.push_str(&format!(
            "{}. **{}**\n   {}\n   URL: {}\n\n",
            i + 1,
            h.title,
            snippet,
            h.url
        ));
    }
    out
}

// ── Serper.dev response types ────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct SerperSearchResponse {
    organic: Vec<SerperOrganicResult>,
}

#[derive(Debug, Deserialize)]
struct SerperOrganicResult {
    title: String,
    snippet: String,
    link: String,
}

// ── SerpAPI response types ───────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct SerpApiSearchResponse {
    organic_results: Vec<SerpApiOrganicResult>,
}

#[derive(Debug, Deserialize)]
struct SerpApiOrganicResult {
    title: String,
    snippet: Option<String>,
    link: String,
}

// ── DuckDuckGo response parsing ──────────────────────────────────────────

async fn search_duckduckgo(
    client: &Client,
    query: &str,
    count: usize,
) -> Result<Vec<SearchHit>, ToolError> {
    let url = format!(
        "https://html.duckduckgo.com/html/?q={}",
        urlencoding::encode(query)
    );

    let response = client
        .get(&url)
        .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64)")
        .send()
        .await
        .map_err(|e| ToolError::Message(format!("DuckDuckGo request failed: {}", e)))?;

    check_status(&response, "DuckDuckGo").await?;

    let html = response
        .text()
        .await
        .map_err(|e| ToolError::Message(format!("DuckDuckGo response read failed: {}", e)))?;

    parse_duckduckgo_html(&html, count)
}

fn parse_duckduckgo_html(html: &str, count: usize) -> Result<Vec<SearchHit>, ToolError> {
    let doc = dom_query::Document::from(html);
    let mut hits = Vec::new();

    for node in doc.select(".result").iter() {
        if hits.len() >= count {
            break;
        }

        let title_sel = node.select(".result__a").iter().next();
        let raw_href = title_sel
            .as_ref()
            .and_then(|n| n.attr("href"))
            .unwrap_or_default();
        let url = extract_duckduckgo_url(&raw_href);
        let title = title_sel
            .map(|n| n.text().trim().to_string())
            .unwrap_or_default();
        let snippet = node
            .select(".result__snippet")
            .iter()
            .next()
            .map(|n| n.text().trim().to_string())
            .unwrap_or_default();

        if !url.is_empty() && !title.is_empty() {
            hits.push(SearchHit {
                title,
                snippet,
                url,
            });
        }
    }

    Ok(hits)
}

/// Extract the real target URL from a DuckDuckGo redirect link.
/// DuckDuckGo wraps result links like `//duckduckgo.com/l/?uddg=...`.
fn extract_duckduckgo_url(href: &str) -> String {
    if href.is_empty() {
        return String::new();
    }

    if let Some(query_start) = href.find('?') {
        let query = &href[query_start + 1..];
        for pair in query.split('&') {
            if let Some((key, value)) = pair.split_once('=') {
                if key == "uddg" {
                    if let Ok(decoded) = urlencoding::decode(value) {
                        return decoded.into_owned();
                    }
                }
            }
        }
    }

    if href.starts_with("//") {
        return format!("https:{}", href);
    }

    href.to_string()
}

// ── Brave response types ─────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct BraveSearchResponse {
    web: BraveWebResults,
}

#[derive(Debug, Deserialize)]
struct BraveWebResults {
    results: Vec<BraveResult>,
}

#[derive(Debug, Deserialize)]
struct BraveResult {
    title: String,
    description: String,
    url: String,
}

async fn search_brave(
    client: &Client,
    api_key: &str,
    query: &str,
    count: usize,
) -> Result<Vec<SearchHit>, ToolError> {
    let url = format!(
        "https://api.search.brave.com/res/v1/web/search?q={}&count={}",
        urlencoding::encode(query),
        count
    );

    let resp: BraveSearchResponse = send_get(
        client,
        &url,
        Some(("X-Subscription-Token", api_key)),
        "Brave",
    )
    .await?;

    Ok(resp
        .web
        .results
        .into_iter()
        .map(|r| SearchHit {
            title: r.title,
            snippet: r.description,
            url: r.url,
        })
        .collect())
}

// ── Tavily response types ────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct TavilySearchResponse {
    results: Vec<TavilyResult>,
}

#[derive(Debug, Deserialize)]
struct TavilyResult {
    title: String,
    content: String,
    url: String,
}

async fn search_tavily(
    client: &Client,
    api_key: &str,
    query: &str,
    count: usize,
) -> Result<Vec<SearchHit>, ToolError> {
    let body = serde_json::json!({
        "api_key": api_key,
        "query": query,
        "max_results": count,
        "search_depth": "basic"
    });

    let resp: TavilySearchResponse = send_post_json(
        client,
        "https://api.tavily.com/search",
        &body,
        None,
        "Tavily",
    )
    .await?;

    Ok(resp
        .results
        .into_iter()
        .map(|r| SearchHit {
            title: r.title,
            snippet: r.content,
            url: r.url,
        })
        .collect())
}

// ── Exa response types ───────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ExaSearchResponse {
    results: Vec<ExaResult>,
}

#[derive(Debug, Deserialize)]
struct ExaResult {
    title: Option<String>,
    text: Option<String>,
    url: String,
}

async fn search_exa(
    client: &Client,
    api_key: &str,
    query: &str,
    count: usize,
) -> Result<Vec<SearchHit>, ToolError> {
    let body = serde_json::json!({
        "query": query,
        "numResults": count,
        "contents": { "text": true }
    });

    let resp: ExaSearchResponse = send_post_json(
        client,
        "https://api.exa.ai/search",
        &body,
        Some(("x-api-key", api_key)),
        "Exa",
    )
    .await?;

    Ok(resp
        .results
        .into_iter()
        .map(|r| SearchHit {
            title: r.title.unwrap_or_else(|| "No title".to_string()),
            snippet: r
                .text
                .map(|t| t.chars().take(MAX_SNIPPET_LEN).collect())
                .unwrap_or_else(|| "No description".to_string()),
            url: r.url,
        })
        .collect())
}

// ── Firecrawl response types ─────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct FirecrawlSearchResponse {
    data: Vec<FirecrawlResult>,
}

#[derive(Debug, Deserialize)]
struct FirecrawlResult {
    title: Option<String>,
    description: Option<String>,
    url: String,
}

async fn search_firecrawl(
    client: &Client,
    api_key: &str,
    query: &str,
    count: usize,
) -> Result<Vec<SearchHit>, ToolError> {
    let body = serde_json::json!({
        "query": query,
        "limit": count
    });

    let resp: FirecrawlSearchResponse = send_post_json(
        client,
        "https://api.firecrawl.dev/v1/search",
        &body,
        Some(("Authorization", &format!("Bearer {}", api_key))),
        "Firecrawl",
    )
    .await?;

    Ok(resp
        .data
        .into_iter()
        .map(|r| SearchHit {
            title: r.title.unwrap_or_else(|| "No title".to_string()),
            snippet: r
                .description
                .unwrap_or_else(|| "No description".to_string()),
            url: r.url,
        })
        .collect())
}

// ── Shared HTTP helpers ──────────────────────────────────────────────────

async fn send_get<T: serde::de::DeserializeOwned>(
    client: &Client,
    url: &str,
    auth_header: Option<(&str, &str)>,
    provider_name: &str,
) -> Result<T, ToolError> {
    let mut req = client.get(url).header("Accept", "application/json");

    if let Some((key, value)) = auth_header {
        req = req.header(key, value);
    }

    let response = req.send().await.map_err(|e| {
        ToolError::Message(format!(
            "{} API request failed: {}",
            provider_name,
            e.without_url()
        ))
    })?;

    check_status(&response, provider_name).await?;

    response.json::<T>().await.map_err(|e| {
        ToolError::Message(format!(
            "Failed to parse {} API response: {}",
            provider_name, e
        ))
    })
}

async fn send_post_json<T: serde::de::DeserializeOwned>(
    client: &Client,
    url: &str,
    body: &serde_json::Value,
    auth_header: Option<(&str, &str)>,
    provider_name: &str,
) -> Result<T, ToolError> {
    let mut req = client
        .post(url)
        .header("Content-Type", "application/json")
        .json(body);

    if let Some((key, value)) = auth_header {
        req = req.header(key, value);
    }

    let response = req.send().await.map_err(|e| {
        ToolError::Message(format!(
            "{} API request failed: {}",
            provider_name,
            e.without_url()
        ))
    })?;

    check_status(&response, provider_name).await?;

    response.json::<T>().await.map_err(|e| {
        ToolError::Message(format!(
            "Failed to parse {} API response: {}",
            provider_name, e
        ))
    })
}

async fn check_status(response: &reqwest::Response, provider_name: &str) -> Result<(), ToolError> {
    if !response.status().is_success() {
        return Err(ToolError::Message(format!(
            "{} API error (status {})",
            provider_name,
            response.status()
        )));
    }
    Ok(())
}

// ── Existing provider implementations ────────────────────────────────────

async fn search_serper(
    client: &Client,
    api_key: &str,
    query: &str,
    count: usize,
) -> Result<Vec<SearchHit>, ToolError> {
    let body = serde_json::json!({
        "q": query,
        "num": count,
    });

    let resp: SerperSearchResponse = send_post_json(
        client,
        "https://google.serper.dev/search",
        &body,
        Some(("X-API-KEY", api_key)),
        "Serper",
    )
    .await?;

    Ok(resp
        .organic
        .into_iter()
        .map(|r| SearchHit {
            title: r.title,
            snippet: r.snippet,
            url: r.link,
        })
        .collect())
}

async fn search_serpapi(
    client: &Client,
    api_key: &str,
    query: &str,
    count: usize,
) -> Result<Vec<SearchHit>, ToolError> {
    let url = format!(
        "https://serpapi.com/search?api_key={}&engine=google&q={}&num={}",
        api_key,
        urlencoding::encode(query),
        count
    );

    let resp: SerpApiSearchResponse = send_get(client, &url, None, "SerpAPI").await?;

    Ok(resp
        .organic_results
        .into_iter()
        .map(|r| SearchHit {
            title: r.title,
            snippet: r.snippet.unwrap_or_else(|| "No description".to_string()),
            url: r.link,
        })
        .collect())
}

/// Reads `env_var`; on success runs `f`, otherwise reports a "not set" error.
async fn with_api_key<F, Fut>(env_var: &str, f: F) -> Result<Vec<SearchHit>, ToolError>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<SearchHit>, ToolError>>,
{
    match std::env::var(env_var) {
        Ok(api_key) => f(api_key).await,
        Err(_) => Err(ToolError::Message(format!("{} not set", env_var))),
    }
}

// ── Tool implementation ──────────────────────────────────────────────────

pub fn register(api: &mut dyn ExtensionApi) {
    api.register_tool(ToolDefinition {
        name: "web_search".into(),
        label: "Web Search".into(),
        description: "Search the web for current information. Supported providers: duckduckgo (default), serper, serpapi, brave, tavily, exa, firecrawl. Configure via CONGA_SEARCH_PROVIDER and the corresponding CONGA_<PROVIDER>_API_KEY environment variable.".into(),
        parameters: serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "Search query" },
                "count": { "type": "number", "description": "Number of results to return (default 5)", "default": 5 }
            },
            "required": ["query"]
        }),
        risk: RiskLevel::High,
        execute: Arc::new(move |ctx| {
            Box::pin(async move {
                // Built per call so the runtime tool proxy (desktop UI /
                // CONGA_TOOL_PROXY) applies without re-registering.
                let client = conga_host::apply_tool_proxy(Client::builder())
                    .build()
                    .map_err(|e| ToolError::Message(format!("client build failed: {e}")))?;
                let query = ctx.args["query"].as_str().unwrap_or_default();
                let count = ctx.args["count"].as_u64().unwrap_or(5) as usize;

                info!("Web search: query='{}', count={}", query, count);

                let provider_name = std::env::var("CONGA_SEARCH_PROVIDER").unwrap_or_else(|_| "duckduckgo".to_string());

                let hits_result = match provider_name.to_lowercase().as_str() {
                    "serper" => {
                        with_api_key("CONGA_SERPER_API_KEY", |key| async move {
                            search_serper(&client, &key, query, count).await
                        })
                        .await
                    }
                    "serpapi" => {
                        with_api_key("CONGA_SERPAPI_API_KEY", |key| async move {
                            search_serpapi(&client, &key, query, count).await
                        })
                        .await
                    }
                    "duckduckgo" => search_duckduckgo(&client, query, count).await,
                    "brave" => {
                        with_api_key("CONGA_BRAVE_API_KEY", |key| async move {
                            search_brave(&client, &key, query, count).await
                        })
                        .await
                    }
                    "tavily" => {
                        with_api_key("CONGA_TAVILY_API_KEY", |key| async move {
                            search_tavily(&client, &key, query, count).await
                        })
                        .await
                    }
                    "exa" => {
                        with_api_key("CONGA_EXA_API_KEY", |key| async move {
                            search_exa(&client, &key, query, count).await
                        })
                        .await
                    }
                    "firecrawl" => {
                        with_api_key("CONGA_FIRECRAWL_API_KEY", |key| async move {
                            search_firecrawl(&client, &key, query, count).await
                        })
                        .await
                    }
                    _ => Err(ToolError::Message(format!(
                        "Unknown search provider: {}. Use 'serper', 'serpapi', 'duckduckgo', 'brave', 'tavily', 'exa', or 'firecrawl'.", provider_name
                    ))),
                };

                match hits_result {
                    Ok(hits) => Ok(ToolResult {
                        content: vec![ContentBlock::text(format_hits(&hits))],
                        details: serde_json::json!({ "result_count": hits.len() }),
                        is_error: false,
                    }),
                    Err(e) => {
                        warn!("Search failed: {}", e);
                        Ok(ToolResult {
                            content: vec![ContentBlock::text(format!("Search failed: {}", e))],
                            details: serde_json::json!({}),
                            is_error: true,
                        })
                    }
                }
            })
        }),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Local HTTP fixtures ─────────────────────────────────────
    // Every test in this module talks to a LOCAL server (127.0.0.1) or to a
    // local capture proxy: no test ever reaches a real search vendor.
    // `no_proxy()` is mandatory here: the machine runs with
    // HTTP_PROXY/HTTPS_PROXY set, and a proxied request would leave the
    // machine (and hit the real vendor through the proxy).

    /// A request as the fake server saw it.
    #[derive(Debug, Default)]
    struct CapturedRequest {
        method: String,
        target: String,
        headers: Vec<(String, String)>,
        body: String,
    }

    impl CapturedRequest {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        }
    }

    /// A client that never uses the ambient proxy configuration.
    fn local_client() -> Client {
        Client::builder().no_proxy().build().expect("client builds")
    }

    fn find_sub(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    /// Serve exactly one request with `status`/`body`, and hand the captured
    /// request back over a oneshot channel.
    async fn spawn_http_server(
        status: u16,
        body: &'static str,
    ) -> (String, tokio::sync::oneshot::Receiver<CapturedRequest>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf: Vec<u8> = Vec::new();
            let mut tmp = [0u8; 4096];
            let head_end = loop {
                let n = sock.read(&mut tmp).await.unwrap();
                if n == 0 {
                    break buf.len();
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some(p) = find_sub(&buf, b"\r\n\r\n") {
                    break p + 4;
                }
            };
            let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
            let mut lines = head.lines();
            let request_line = lines.next().unwrap_or_default().to_string();
            let mut parts = request_line.split_whitespace();
            let method = parts.next().unwrap_or_default().to_string();
            let target = parts.next().unwrap_or_default().to_string();
            let mut headers: Vec<(String, String)> = Vec::new();
            let mut content_length = 0usize;
            for line in lines {
                if let Some((k, v)) = line.split_once(':') {
                    let k = k.trim().to_string();
                    let v = v.trim().to_string();
                    if k.eq_ignore_ascii_case("content-length") {
                        content_length = v.parse().unwrap_or(0);
                    }
                    headers.push((k, v));
                }
            }
            let mut body_bytes = buf[head_end..].to_vec();
            while body_bytes.len() < content_length {
                let n = sock.read(&mut tmp).await.unwrap();
                if n == 0 {
                    break;
                }
                body_bytes.extend_from_slice(&tmp[..n]);
            }
            let _ = tx.send(CapturedRequest {
                method,
                target,
                headers,
                body: String::from_utf8_lossy(&body_bytes).to_string(),
            });
            let resp = format!(
                "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
            let _ = sock.flush().await;
        });
        (format!("http://{addr}"), rx)
    }

    /// A local proxy that records the request line of the first connection.
    /// Used to observe WHICH host a provider function talks to without any
    /// traffic leaving the machine: the proxy grants the CONNECT tunnel and
    /// closes, so the client's TLS handshake fails right after.
    async fn spawn_capture_proxy() -> (String, tokio::sync::oneshot::Receiver<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut head = String::new();
            let mut tmp = [0u8; 1024];
            loop {
                let n = sock.read(&mut tmp).await.unwrap();
                if n == 0 {
                    break;
                }
                head.push_str(&String::from_utf8_lossy(&tmp[..n]));
                if head.contains("\r\n\r\n") {
                    break;
                }
            }
            let _ = tx.send(head.clone());
            let _ = sock
                .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .await;
        });
        (format!("http://{addr}"), rx)
    }

    /// Run a provider function against the capture proxy and return the
    /// request line the proxy saw plus the error the function reported.
    async fn probe_provider<F, Fut>(f: F) -> (String, String)
    where
        F: FnOnce(Client) -> Fut,
        Fut: std::future::Future<Output = Result<Vec<SearchHit>, ToolError>>,
    {
        let (proxy_url, rx) = spawn_capture_proxy().await;
        // An explicit proxy disables the ambient system proxy (reqwest).
        let client = Client::builder()
            .proxy(reqwest::Proxy::all(&proxy_url).unwrap())
            .build()
            .expect("client builds");
        let err = match f(client).await {
            Ok(_) => panic!("expected the tunnel-less proxy to fail the request"),
            Err(e) => e.to_string(),
        };
        let head = tokio::time::timeout(std::time::Duration::from_secs(5), rx)
            .await
            .expect("proxy never saw the request")
            .expect("proxy task died");
        (head, err)
    }

    // ── Vendor JSON shape contracts ──────────────────────────────
    // Each test pins the serde field mapping against a canned response in the
    // vendor's real shape. A vendor renaming a field fails here, not in prod.

    #[test]
    fn serper_response_maps_fields() {
        let resp: SerperSearchResponse = serde_json::from_str(
            r#"{"organic":[{"title":"Rust Lang","snippet":"A language","link":"https://rust-lang.org"}]}"#,
        )
        .unwrap();
        assert_eq!(resp.organic.len(), 1);
        assert_eq!(resp.organic[0].title, "Rust Lang");
        assert_eq!(resp.organic[0].snippet, "A language");
        assert_eq!(resp.organic[0].link, "https://rust-lang.org");
    }

    #[test]
    fn serpapi_response_optional_snippet() {
        let resp: SerpApiSearchResponse = serde_json::from_str(
            r#"{"organic_results":[{"title":"T","link":"https://t.example"}]}"#,
        )
        .unwrap();
        assert_eq!(resp.organic_results.len(), 1);
        assert_eq!(resp.organic_results[0].snippet, None);
        assert_eq!(resp.organic_results[0].title, "T");
        assert_eq!(resp.organic_results[0].link, "https://t.example");
    }

    #[test]
    fn brave_response_maps_fields() {
        let resp: BraveSearchResponse = serde_json::from_str(
            r#"{"web":{"results":[{"title":"B","description":"D","url":"https://b.example"}]}}"#,
        )
        .unwrap();
        assert_eq!(resp.web.results.len(), 1);
        assert_eq!(resp.web.results[0].title, "B");
        assert_eq!(resp.web.results[0].description, "D");
        assert_eq!(resp.web.results[0].url, "https://b.example");
    }

    #[test]
    fn tavily_response_maps_fields() {
        let resp: TavilySearchResponse = serde_json::from_str(
            r#"{"results":[{"title":"V","content":"C","url":"https://v.example"}]}"#,
        )
        .unwrap();
        assert_eq!(resp.results.len(), 1);
        assert_eq!(resp.results[0].title, "V");
        assert_eq!(resp.results[0].content, "C");
        assert_eq!(resp.results[0].url, "https://v.example");
    }

    #[test]
    fn exa_response_optional_fields() {
        let resp: ExaSearchResponse =
            serde_json::from_str(r#"{"results":[{"url":"https://e.example"}]}"#).unwrap();
        assert_eq!(resp.results.len(), 1);
        assert_eq!(resp.results[0].title, None);
        assert_eq!(resp.results[0].text, None);
        assert_eq!(resp.results[0].url, "https://e.example");
    }

    #[test]
    fn firecrawl_response_optional_fields() {
        let resp: FirecrawlSearchResponse =
            serde_json::from_str(r#"{"data":[{"url":"https://f.example"}]}"#).unwrap();
        assert_eq!(resp.data.len(), 1);
        assert_eq!(resp.data[0].title, None);
        assert_eq!(resp.data[0].description, None);
        assert_eq!(resp.data[0].url, "https://f.example");
    }

    // ── DuckDuckGo HTML parsing ──────────────────────────────────

    fn ddg_fixture() -> &'static str {
        r#"<html><body>
          <div class="result">
            <h2 class="result__title">
              <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage&amp;rut=abc">Example Page</a>
            </h2>
            <a class="result__snippet" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpage">The example snippet.</a>
          </div>
          <div class="result">
            <h2 class="result__title">
              <a class="result__a" href="https://plain.example.org/direct">Plain Link</a>
            </h2>
            <a class="result__snippet" href="https://plain.example.org/direct">Second snippet.</a>
          </div>
          <div class="result">
            <h2 class="result__title"><span>No Link Here</span></h2>
          </div>
        </body></html>"#
    }

    #[test]
    fn parse_duckduckgo_html_extracts_results() {
        let hits = parse_duckduckgo_html(ddg_fixture(), 10).unwrap();
        assert_eq!(hits.len(), 2, "the link-less result must be skipped");
        assert_eq!(hits[0].title, "Example Page");
        assert_eq!(hits[0].url, "https://example.com/page");
        assert_eq!(hits[0].snippet, "The example snippet.");
        assert_eq!(hits[1].title, "Plain Link");
        assert_eq!(hits[1].url, "https://plain.example.org/direct");
    }

    #[test]
    fn parse_duckduckgo_html_respects_count() {
        let hits = parse_duckduckgo_html(ddg_fixture(), 1).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Example Page");
    }

    #[test]
    fn extract_duckduckgo_url_decodes_uddg() {
        assert_eq!(
            extract_duckduckgo_url(
                "//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa%3Fb%3D1&rut=x"
            ),
            "https://example.com/a?b=1"
        );
        // Plain https link passes through untouched.
        assert_eq!(
            extract_duckduckgo_url("https://plain.example.org/direct"),
            "https://plain.example.org/direct"
        );
        // Protocol-relative link gets https:.
        assert_eq!(
            extract_duckduckgo_url("//protocol.example.org"),
            "https://protocol.example.org"
        );
        assert_eq!(extract_duckduckgo_url(""), "");
    }

    // ── Output formatting ────────────────────────────────────────

    #[test]
    fn format_hits_empty() {
        assert_eq!(format_hits(&[]), "No results found.");
    }

    #[test]
    fn format_hits_truncates_long_snippets() {
        let long = "x".repeat(400);
        let out = format_hits(&[SearchHit {
            title: "T".into(),
            snippet: long.clone(),
            url: "https://u".into(),
        }]);
        assert!(
            out.contains("..."),
            "long snippet must be truncated with ellipsis"
        );
        assert!(!out.contains(&long), "full snippet must not appear");
        assert!(out.contains("1. **T**"));
        assert!(out.contains("https://u"));
    }

    /// Wiring proof: the tool must build its HTTP client at execute time so
    /// the runtime tool proxy applies. A minimal in-test HTTP proxy observes
    /// the tool's traffic. DuckDuckGo is fetched over https, so a conforming
    /// client opens a CONNECT tunnel through the proxy (absolute-form GET
    /// only happens for plain-http targets); the fake proxy grants the
    /// tunnel but speaks no TLS, so the search surfaces as a tool error —
    /// proving the request went through the proxy, never direct. The
    /// response-parsing path is covered by the `parse_duckduckgo_html`
    /// tests below.
    #[tokio::test]
    async fn web_search_goes_through_tool_proxy() {
        use conga::{ExtensionApiImpl, ToolCallCtx, ToolContext};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
            // Grant the CONNECT tunnel, then close: no TLS is spoken, so a
            // client that (incorrectly) connected directly would never reach
            // this socket at all.
            sock.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .await
                .unwrap();
            head
        });

        // Mutates process env deliberately: the provider is selected at
        // execute time. No other test in this crate reads env vars.
        std::env::set_var("CONGA_SEARCH_PROVIDER", "duckduckgo");
        conga_host::set_tool_proxy(Some(&format!("http://{proxy_addr}"))).unwrap();

        let mut api = ExtensionApiImpl::new();
        super::register(&mut api);
        assert_eq!(api.tools.len(), 1);
        assert_eq!(api.tools[0].name, "web_search");
        let tool = api.tools.remove(0);
        let ctx = ToolCallCtx {
            tool_call_id: "t1".into(),
            args: serde_json::json!({"query": "rust language", "count": 2}),
            signal: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            ctx: ToolContext {
                cwd: ".".into(),
                env: std::collections::HashMap::new(),
                session_id: "t".into(),
                state_dir: ".".into(),
            },
        };
        let result = (tool.execute)(ctx).await.unwrap();

        // Cleanup before assertions so a failed assert can't leak state.
        conga_host::set_tool_proxy(None).unwrap();
        std::env::remove_var("CONGA_SEARCH_PROVIDER");

        assert!(result.is_error, "the TLS-less tunnel must fail: {result:?}");
        match &result.content[0] {
            conga::ContentBlock::Text { text } => {
                assert!(text.contains("Search failed"), "got: {text}");
                assert!(text.contains("https://html.duckduckgo.com/"), "got: {text}");
            }
            _ => panic!("expected text content"),
        }
        // Bounded: with a reverted (registration-time, proxy-less) client the
        // request goes direct and this proxy socket is never reached. Without
        // the timeout the await below would block forever — the assertions
        // above can all pass on a direct-connection failure, so this join is
        // the real wiring check and must fail fast with a diagnosis.
        let head = match tokio::time::timeout(std::time::Duration::from_secs(30), server).await {
            Ok(joined) => joined.unwrap(),
            Err(_) => panic!(
                "proxy socket never reached within 30s: the request did not go \
                 through the tool proxy (is the client still built per call?). The \
                 content assertions above passed on the direct-connection error: \
                 is_error={}, content={:?}",
                result.is_error, result.content
            ),
        };
        assert!(
            head.starts_with("CONNECT html.duckduckgo.com:443 HTTP/1.1"),
            "proxy saw: {head}"
        );
    }

    // ── Shared HTTP helpers (send_get / send_post_json / check_status) ──

    #[tokio::test]
    async fn send_get_sends_method_target_and_auth_header() {
        let (base, rx) = spawn_http_server(
            200,
            r#"{"organic":[{"title":"Rust Lang","snippet":"A language","link":"https://rust-lang.org"}]}"#,
        )
        .await;
        let client = local_client();
        let url = format!("{base}/search?q=rust&count=3");

        let resp: SerperSearchResponse =
            send_get(&client, &url, Some(("X-API-KEY", "secret-key")), "Serper")
                .await
                .expect("provider response parses");

        assert_eq!(resp.organic.len(), 1);
        assert_eq!(resp.organic[0].title, "Rust Lang");

        let req = rx.await.expect("server task died");
        assert_eq!(req.method, "GET");
        assert_eq!(req.target, "/search?q=rust&count=3");
        assert_eq!(req.header("x-api-key"), Some("secret-key"));
        assert_eq!(req.header("accept"), Some("application/json"));
        assert_eq!(req.body, "", "a GET carries no body");
    }

    #[tokio::test]
    async fn send_get_without_auth_header_sends_none() {
        let (base, rx) = spawn_http_server(200, r#"{"organic_results":[]}"#).await;
        let client = local_client();
        let url = format!("{base}/search?api_key=k&engine=google&q=rust&num=5");

        let resp: SerpApiSearchResponse = send_get(&client, &url, None, "SerpAPI")
            .await
            .expect("provider response parses");
        assert!(resp.organic_results.is_empty());

        let req = rx.await.expect("server task died");
        assert_eq!(req.method, "GET");
        assert_eq!(
            req.target, "/search?api_key=k&engine=google&q=rust&num=5",
            "the query string is forwarded verbatim"
        );
        assert_eq!(req.header("authorization"), None);
    }

    #[tokio::test]
    async fn send_get_reports_http_error_status() {
        let (base, _rx) = spawn_http_server(500, "{}").await;
        let client = local_client();
        let err =
            send_get::<SerperSearchResponse>(&client, &format!("{base}/search"), None, "Serper")
                .await
                .expect_err("a 500 must fail");
        assert!(
            err.to_string().contains("Serper API error (status 500"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn send_get_reports_parse_error() {
        let (base, _rx) = spawn_http_server(200, "this is not json").await;
        let client = local_client();
        let err =
            send_get::<SerperSearchResponse>(&client, &format!("{base}/search"), None, "Serper")
                .await
                .expect_err("a broken body must fail");
        assert!(
            err.to_string()
                .contains("Failed to parse Serper API response"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn send_get_reports_transport_error() {
        // A port nobody listens on: the connection is refused locally.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let client = local_client();
        let err = send_get::<SerperSearchResponse>(
            &client,
            &format!("http://{addr}/search"),
            None,
            "Serper",
        )
        .await
        .expect_err("an unreachable host must fail");
        assert!(
            err.to_string().contains("Serper API request failed"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn send_post_json_sends_body_content_type_and_auth_header() {
        let (base, rx) = spawn_http_server(
            200,
            r#"{"results":[{"title":"T","content":"C","url":"https://v.example"}]}"#,
        )
        .await;
        let client = local_client();
        let body = serde_json::json!({
            "api_key": "k",
            "query": "rust 语言",
            "max_results": 3,
            "search_depth": "basic"
        });

        let resp: TavilySearchResponse =
            send_post_json(&client, &format!("{base}/search"), &body, None, "Tavily")
                .await
                .expect("provider response parses");
        assert_eq!(resp.results[0].title, "T");

        let req = rx.await.expect("server task died");
        assert_eq!(req.method, "POST");
        assert_eq!(req.target, "/search");
        assert_eq!(req.header("content-type"), Some("application/json"));
        let sent: serde_json::Value = serde_json::from_str(&req.body).expect("body is JSON");
        assert_eq!(sent, body, "the JSON body is forwarded verbatim");
        assert_eq!(
            sent["query"], "rust 语言",
            "unicode survives the round trip"
        );
    }

    #[tokio::test]
    async fn send_post_json_reports_http_error_status() {
        let (base, _rx) = spawn_http_server(400, "{}").await;
        let client = local_client();
        let err = send_post_json::<TavilySearchResponse>(
            &client,
            &format!("{base}/search"),
            &serde_json::json!({"query": "x"}),
            None,
            "Tavily",
        )
        .await
        .expect_err("a 400 must fail");
        assert!(
            err.to_string().contains("Tavily API error (status 400"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn send_post_json_reports_parse_error() {
        let (base, _rx) = spawn_http_server(200, "<html>nope</html>").await;
        let client = local_client();
        let err = send_post_json::<TavilySearchResponse>(
            &client,
            &format!("{base}/search"),
            &serde_json::json!({"query": "x"}),
            None,
            "Tavily",
        )
        .await
        .expect_err("a broken body must fail");
        assert!(
            err.to_string()
                .contains("Failed to parse Tavily API response"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn check_status_accepts_success_and_rejects_failures() {
        let client = local_client();
        let (ok_base, _rx) = spawn_http_server(200, "{}").await;
        let resp = client.get(format!("{ok_base}/ok")).send().await.unwrap();
        assert!(check_status(&resp, "Brave").await.is_ok());

        let (missing_base, _rx2) = spawn_http_server(404, "{}").await;
        let resp = client
            .get(format!("{missing_base}/missing"))
            .send()
            .await
            .unwrap();
        let err = check_status(&resp, "Brave")
            .await
            .expect_err("404 must fail");
        assert!(
            err.to_string().contains("Brave API error (status 404"),
            "{err}"
        );

        let (boom_base, _rx3) = spawn_http_server(503, "{}").await;
        let resp = client
            .get(format!("{boom_base}/boom"))
            .send()
            .await
            .unwrap();
        let err = check_status(&resp, "Brave")
            .await
            .expect_err("503 must fail");
        assert!(
            err.to_string().contains("Brave API error (status 503"),
            "{err}"
        );
    }

    // ── API-key handling ────────────────────────────────────────

    #[tokio::test]
    async fn with_api_key_reports_a_missing_env_var() {
        let res = with_api_key("CONGA_TEST_UNSET_API_KEY_PROBE", |_key| async {
            Ok(Vec::new())
        })
        .await;
        // `SearchHit` has no Debug impl: unwrap the error by hand.
        let err = match res {
            Ok(_) => panic!("an unset key must fail"),
            Err(e) => e,
        };
        assert!(
            err.to_string()
                .contains("CONGA_TEST_UNSET_API_KEY_PROBE not set"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn with_api_key_hands_the_env_value_to_the_provider() {
        // PATH is always set: reading it proves the value is forwarded
        // verbatim without touching the process environment.
        let expected = std::env::var("PATH").expect("PATH is set");
        let hits = with_api_key("PATH", |key| async move {
            Ok(vec![SearchHit {
                title: key,
                snippet: "s".into(),
                url: "u".into(),
            }])
        })
        .await
        .expect("a set key must be forwarded");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, expected);
    }

    // ── Provider entry points ───────────────────────────────────
    // The vendor hosts are hard-coded and https-only, so the happy path
    // cannot be reached without a TLS server. These tests pin the observable
    // contract that IS reachable locally: which host each provider targets
    // and which provider name appears in the failure.

    #[tokio::test]
    async fn search_duckduckgo_targets_duckduckgo_with_an_encoded_query() {
        let (head, err) =
            probe_provider(
                |client| async move { search_duckduckgo(&client, "rust 语言", 3).await },
            )
            .await;
        assert!(
            head.starts_with("CONNECT html.duckduckgo.com:443 HTTP/1.1"),
            "proxy saw: {head}"
        );
        assert!(err.contains("DuckDuckGo request failed"), "{err}");
        assert!(
            err.contains("html.duckduckgo.com/html/?q=rust%20"),
            "the query is percent-encoded: {err}"
        );
    }

    #[tokio::test]
    async fn search_brave_targets_brave_and_names_the_provider() {
        let (head, err) =
            probe_provider(|client| async move { search_brave(&client, "k", "q", 3).await }).await;
        assert!(
            head.starts_with("CONNECT api.search.brave.com:443 HTTP/1.1"),
            "proxy saw: {head}"
        );
        assert!(err.contains("Brave API request failed"), "{err}");
    }

    #[tokio::test]
    async fn search_tavily_targets_tavily_and_names_the_provider() {
        let (head, err) =
            probe_provider(|client| async move { search_tavily(&client, "k", "q", 3).await }).await;
        assert!(
            head.starts_with("CONNECT api.tavily.com:443 HTTP/1.1"),
            "proxy saw: {head}"
        );
        assert!(err.contains("Tavily API request failed"), "{err}");
    }

    #[tokio::test]
    async fn search_exa_targets_exa_and_names_the_provider() {
        let (head, err) =
            probe_provider(|client| async move { search_exa(&client, "k", "q", 3).await }).await;
        assert!(
            head.starts_with("CONNECT api.exa.ai:443 HTTP/1.1"),
            "proxy saw: {head}"
        );
        assert!(err.contains("Exa API request failed"), "{err}");
    }

    #[tokio::test]
    async fn search_firecrawl_targets_firecrawl_and_names_the_provider() {
        let (head, err) =
            probe_provider(|client| async move { search_firecrawl(&client, "k", "q", 3).await })
                .await;
        assert!(
            head.starts_with("CONNECT api.firecrawl.dev:443 HTTP/1.1"),
            "proxy saw: {head}"
        );
        assert!(err.contains("Firecrawl API request failed"), "{err}");
    }

    #[tokio::test]
    async fn search_serper_targets_serper_and_names_the_provider() {
        let (head, err) =
            probe_provider(|client| async move { search_serper(&client, "k", "q", 3).await }).await;
        assert!(
            head.starts_with("CONNECT google.serper.dev:443 HTTP/1.1"),
            "proxy saw: {head}"
        );
        assert!(err.contains("Serper API request failed"), "{err}");
    }

    #[tokio::test]
    async fn search_serpapi_targets_serpapi_and_names_the_provider() {
        let (head, err) =
            probe_provider(|client| async move { search_serpapi(&client, "k", "q", 3).await })
                .await;
        assert!(
            head.starts_with("CONNECT serpapi.com:443 HTTP/1.1"),
            "proxy saw: {head}"
        );
        assert!(err.contains("SerpAPI API request failed"), "{err}");
    }

    // ── Tool registration ───────────────────────────────────────

    #[test]
    fn register_exposes_the_web_search_tool() {
        use conga::{ExtensionApiImpl, RiskLevel};

        let mut api = ExtensionApiImpl::new();
        register(&mut api);
        assert_eq!(api.tools.len(), 1);
        let tool = &api.tools[0];
        assert_eq!(tool.name, "web_search");
        // Searching the web is egress: it must never be auto-approved.
        assert!(matches!(tool.risk, RiskLevel::High));
        assert_eq!(tool.parameters["required"][0], "query");
        assert_eq!(tool.parameters["properties"]["count"]["default"], 5);
    }
}
