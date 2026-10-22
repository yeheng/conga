//! OpenAI-compatible /embeddings client: batching + retry + validation.

use conga::RetryPolicy;
use serde::Deserialize;

use crate::config::ResolvedEmbedding;

#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    #[error("embeddings API HTTP {status}: {detail}")]
    Http {
        status: u16,
        retryable: bool,
        detail: String,
    },
    #[error("embedding 响应条数不符:期望 {want},实得 {got}")]
    Count { want: usize, got: usize },
    #[error("embedding 维度不一致:首批 {first},第 {idx} 条为 {got}")]
    Dim {
        first: usize,
        idx: usize,
        got: usize,
    },
    #[error("网络错误: {0}")]
    Network(#[source] reqwest::Error),
}

impl EmbedError {
    pub fn is_retryable(&self) -> bool {
        match self {
            EmbedError::Http { retryable, .. } => *retryable,
            EmbedError::Network(_) => true,
            _ => false,
        }
    }
}

#[derive(Deserialize)]
struct EmbResponse {
    data: Vec<EmbItem>,
}

#[derive(Deserialize)]
struct EmbItem {
    index: usize,
    embedding: Vec<f32>,
}

pub struct EmbeddingsClient {
    base_url: String,
    api_key: String,
    model: String,
    batch: usize,
    /// Pause between consecutive batch requests (0 = off). Batches are
    /// already the provider's max size; this throttles the request rate so
    /// per-minute quotas (429) are not tripped by back-to-back bursts.
    min_interval_ms: u64,
    client: reqwest::Client,
    retry: RetryPolicy,
}

fn default_retry() -> RetryPolicy {
    // Rate limits (429) on embedding endpoints are per-minute quota windows
    // (e.g. Ark AccountRateLimitExceeded), so 429s need patience measured in
    // tens of seconds, not sub-second bursts: 1+2+4+8+16+30+30 ≈ 91s worst
    // case before giving up. Tests inject their own fast policy.
    RetryPolicy {
        max_retries: 7,
        initial_delay_ms: 1000,
        max_delay_ms: 30000,
        jitter: true,
    }
}

impl EmbeddingsClient {
    pub fn new(cfg: &ResolvedEmbedding) -> Self {
        Self::with_retry(cfg, &cfg.base_url, default_retry())
    }

    pub fn with_base_url(cfg: &ResolvedEmbedding, base_url: &str) -> Self {
        Self::with_retry(cfg, base_url, default_retry())
    }

    pub fn with_retry(cfg: &ResolvedEmbedding, base_url: &str, retry: RetryPolicy) -> Self {
        Self {
            base_url: base_url.to_string(),
            api_key: cfg.api_key.clone(),
            model: cfg.model.clone(),
            batch: cfg.batch.max(1),
            min_interval_ms: cfg.min_interval_ms,
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .build()
                .expect("reqwest client construction with 60s timeout"),
            retry,
        }
    }

    pub async fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        Ok(self
            .embed_batch(&[text.to_string()])
            .await?
            .into_iter()
            .next()
            .unwrap())
    }

    /// Embed texts in batches of `self.batch`, sequentially, pacing
    /// consecutive requests `min_interval_ms` apart when configured.
    pub async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let mut out = Vec::with_capacity(texts.len());
        let total = texts.len().div_ceil(self.batch);
        let started = std::time::Instant::now();
        for (i, chunk) in texts.chunks(self.batch).enumerate() {
            // Throttle before every request except the first: one ingest
            // fires ceil(chunks/batch) requests back-to-back, which is what
            // trips per-minute quotas on providers like Ark.
            if i > 0 && self.min_interval_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(self.min_interval_ms)).await;
            }
            // Per-batch progress only for multi-batch runs: single-batch calls
            // are ad-hoc queries where the line would be noise.
            if total > 1 {
                tracing::info!(
                    batch = i + 1,
                    total,
                    items = chunk.len(),
                    elapsed_s = format!("{:.1}", started.elapsed().as_secs_f32()),
                    "embedding 批次"
                );
            }
            out.extend(self.request_with_retry(chunk).await?);
        }
        Ok(out)
    }

    async fn request_with_retry(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let mut attempt = 0usize;
        loop {
            match self.one_request(texts).await {
                Ok(v) => return Ok(v),
                Err(e) if attempt < self.retry.max_retries && e.is_retryable() => {
                    let mut delay = self
                        .retry
                        .initial_delay_ms
                        .saturating_mul(1u64 << attempt)
                        .min(self.retry.max_delay_ms);
                    if self.retry.jitter {
                        delay += rand_jitter(delay);
                    }
                    tracing::warn!(attempt, delay_ms = delay, "embeddings 重试: {e}");
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn one_request(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let url = format!("{}/embeddings", self.base_url.trim_end_matches('/'));
        tracing::debug!(
            url = %url,
            model = %self.model,
            items = texts.len(),
            chars = texts.iter().map(|t| t.chars().count()).sum::<usize>(),
            "POST embeddings"
        );
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&serde_json::json!({"model": self.model, "input": texts}))
            .send()
            .await
            .map_err(EmbedError::Network)?;
        let status = resp.status();
        if !status.is_success() {
            // Surface the server's error body (e.g. "input limit exceeded:
            // max 10, got 64") instead of swallowing it behind a bare status.
            let body = resp.text().await.unwrap_or_default();
            return Err(EmbedError::Http {
                status: status.as_u16(),
                retryable: status.as_u16() == 429 || status.is_server_error(),
                detail: truncate_chars(body.trim(), 400),
            });
        }
        let body: EmbResponse = resp.json().await.map_err(EmbedError::Network)?;
        let mut items = body.data;
        items.sort_by_key(|i| i.index);
        if items.len() != texts.len() {
            return Err(EmbedError::Count {
                want: texts.len(),
                got: items.len(),
            });
        }
        let dim = items.first().map(|i| i.embedding.len()).unwrap_or(0);
        for (idx, item) in items.iter().enumerate() {
            if item.embedding.len() != dim {
                return Err(EmbedError::Dim {
                    first: dim,
                    idx,
                    got: item.embedding.len(),
                });
            }
        }
        Ok(items.into_iter().map(|i| i.embedding).collect())
    }
}

/// Simple jitter without a rand dep: scale by a cheap per-call factor.
fn rand_jitter(base: u64) -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    base / 4 + nanos % (base / 4 + 1)
}

/// Char-boundary-safe truncation for error bodies (str::truncate would panic
/// mid-char and bodies can be arbitrary JSON).
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::post;
    use axum::{Json, Router};

    /// Retry policy with sub-millisecond delays: the tests must not sleep.
    fn fast_retry(max_retries: usize) -> RetryPolicy {
        RetryPolicy {
            max_retries,
            initial_delay_ms: 1,
            max_delay_ms: 4,
            jitter: false,
        }
    }

    fn cfg(batch: usize) -> ResolvedEmbedding {
        ResolvedEmbedding {
            base_url: "http://127.0.0.1:1/v1".into(),
            api_key: "k".into(),
            model: "mock".into(),
            batch,
            min_interval_ms: 0,
        }
    }

    /// Swap in a proxy-free HTTP client: the machine-wide `HTTP_PROXY`
    /// would otherwise hijack requests aimed at the local mock server.
    fn without_proxy(mut c: EmbeddingsClient) -> EmbeddingsClient {
        c.client = reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .expect("proxy-free client");
        c
    }

    /// Mock that answers every request with a fixed JSON payload (200 OK) —
    /// used to exercise malformed / partial embeddings responses.
    async fn spawn_payload(payload: serde_json::Value) -> String {
        let app = Router::new().route(
            "/v1/embeddings",
            post(move || {
                let payload = payload.clone();
                async move { (StatusCode::OK, Json(payload)).into_response() }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}/v1")
    }

    /// A port nothing listens on (the listener is dropped right away).
    async fn closed_port() -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        port
    }

    // --- error classification ---

    #[tokio::test]
    async fn http_429_and_server_errors_are_retryable() {
        for status in [429u16, 500, 502, 503] {
            let (base, _req) =
                crate::testsupport::spawn_mock_embeddings_error(status, "boom").await;
            let client = without_proxy(EmbeddingsClient::with_retry(&cfg(1), &base, fast_retry(0)));
            let err = client.one_request(&["x".to_string()]).await.unwrap_err();
            assert!(err.is_retryable(), "status {status} must be retryable");
            match err {
                EmbedError::Http { status: s, .. } => assert_eq!(s, status),
                other => panic!("unexpected error for {status}: {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn http_client_errors_are_not_retryable() {
        for status in [400u16, 401, 403, 404, 422] {
            let (base, _req) =
                crate::testsupport::spawn_mock_embeddings_error(status, "boom").await;
            let client = without_proxy(EmbeddingsClient::with_retry(&cfg(1), &base, fast_retry(0)));
            let err = client.one_request(&["x".to_string()]).await.unwrap_err();
            assert!(!err.is_retryable(), "status {status} must not be retried");
            match err {
                EmbedError::Http { status: s, .. } => assert_eq!(s, status),
                other => panic!("unexpected error for {status}: {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn network_failures_are_retryable() {
        let port = closed_port().await;
        let mut c = cfg(1);
        c.base_url = format!("http://127.0.0.1:{port}/v1");
        let client = without_proxy(EmbeddingsClient::with_retry(&c, &c.base_url, fast_retry(0)));
        let err = client.embed_query("x").await.unwrap_err();
        assert!(err.is_retryable(), "transport errors must be retryable");
        assert!(
            matches!(&err, EmbedError::Network(_)),
            "expected a network error, got {err:?}"
        );
    }

    #[test]
    fn count_and_dim_errors_are_not_retryable() {
        let count = EmbedError::Count { want: 2, got: 1 };
        assert!(!count.is_retryable());
        let dim = EmbedError::Dim {
            first: 4,
            idx: 1,
            got: 3,
        };
        assert!(!dim.is_retryable());
    }

    // --- construction ---

    #[test]
    fn default_retry_waits_tens_of_seconds_for_rate_limits() {
        let p = default_retry();
        assert_eq!(p.max_retries, 7, "429s need a patient budget");
        assert_eq!(p.initial_delay_ms, 1000);
        assert_eq!(p.max_delay_ms, 30000);
        assert!(p.jitter, "concurrent workers must not retry in lockstep");
    }

    #[test]
    fn new_uses_the_configured_connection_and_the_default_retry() {
        let c = EmbeddingsClient::new(&cfg(8));
        assert_eq!(c.base_url, "http://127.0.0.1:1/v1");
        assert_eq!(c.api_key, "k");
        assert_eq!(c.model, "mock");
        assert_eq!(c.batch, 8);
        assert_eq!(c.min_interval_ms, 0);
        assert_eq!(c.retry.max_retries, default_retry().max_retries);
        assert_eq!(c.retry.initial_delay_ms, default_retry().initial_delay_ms);
    }

    #[test]
    fn with_base_url_overrides_only_the_url() {
        let c = EmbeddingsClient::with_base_url(&cfg(4), "http://example.test/v9");
        assert_eq!(c.base_url, "http://example.test/v9");
        assert_eq!(c.model, "mock");
        assert_eq!(c.batch, 4);
        assert_eq!(c.retry.max_retries, default_retry().max_retries);
    }

    #[test]
    fn with_retry_keeps_the_given_policy() {
        let p = RetryPolicy {
            max_retries: 2,
            initial_delay_ms: 7,
            max_delay_ms: 9,
            jitter: false,
        };
        let c = EmbeddingsClient::with_retry(&cfg(4), "http://example.test/v1", p.clone());
        assert_eq!(c.retry.max_retries, p.max_retries);
        assert_eq!(c.retry.initial_delay_ms, p.initial_delay_ms);
        assert_eq!(c.retry.max_delay_ms, p.max_delay_ms);
        assert_eq!(c.retry.jitter, p.jitter);
    }

    #[test]
    fn batch_size_is_clamped_to_at_least_one() {
        let c = EmbeddingsClient::new(&cfg(0));
        assert_eq!(c.batch, 1, "a zero batch size would divide by zero");
    }

    // --- embedding ---

    #[tokio::test]
    async fn embed_query_returns_one_vector() {
        let (base, requests) = crate::testsupport::spawn_mock_embeddings(0).await;
        let client = without_proxy(EmbeddingsClient::with_retry(&cfg(4), &base, fast_retry(2)));
        let v = client.embed_query("match").await.unwrap();
        assert_eq!(v, vec![1.0, 0.0, 0.0, 0.0]);
        assert_eq!(requests.load(Ordering::SeqCst), 1, "one query, one request");
    }

    #[tokio::test]
    async fn embed_query_propagates_the_server_error() {
        let (base, _req) = crate::testsupport::spawn_mock_embeddings_error(400, "bad input").await;
        let client = without_proxy(EmbeddingsClient::with_retry(&cfg(4), &base, fast_retry(2)));
        let err = client.embed_query("x").await.unwrap_err();
        assert!(!err.is_retryable());
        assert!(err.to_string().contains("bad input"), "err: {err}");
    }

    #[tokio::test]
    async fn embed_batch_without_texts_makes_no_request() {
        let (base, requests) = crate::testsupport::spawn_mock_embeddings(0).await;
        let client = without_proxy(EmbeddingsClient::with_retry(&cfg(4), &base, fast_retry(2)));
        let out = client.embed_batch(&[]).await.unwrap();
        assert!(out.is_empty());
        assert_eq!(requests.load(Ordering::SeqCst), 0, "no texts, no calls");
    }

    #[tokio::test]
    async fn embed_batch_splits_into_batches_of_the_configured_size() {
        let (base, requests) = crate::testsupport::spawn_mock_embeddings(0).await;
        let client = without_proxy(EmbeddingsClient::with_retry(&cfg(2), &base, fast_retry(2)));
        let texts: Vec<String> = (0..5).map(|i| format!("doc {i}")).collect();
        let out = client.embed_batch(&texts).await.unwrap();
        assert_eq!(out.len(), 5);
        assert!(out.iter().all(|v| v.len() == 4));
        assert_eq!(
            requests.load(Ordering::SeqCst),
            3,
            "5 texts with batch size 2 → 3 requests"
        );
    }

    #[tokio::test]
    async fn embed_batch_preserves_the_input_order() {
        let (base, _req) = crate::testsupport::spawn_mock_embeddings(0).await;
        let client = without_proxy(EmbeddingsClient::with_retry(&cfg(8), &base, fast_retry(2)));
        let texts = vec!["a".to_string(), "bb".to_string(), "ccc".to_string()];
        let out = client.embed_batch(&texts).await.unwrap();
        assert_eq!(out.len(), 3);
        // The mock encodes the text length in the second component.
        assert_eq!(out[0][1], 0.001, "first text must stay first");
        assert_eq!(out[1][1], 0.002);
        assert_eq!(out[2][1], 0.003);
    }

    // --- retry loop ---

    #[tokio::test]
    async fn request_with_retry_retries_after_a_429() {
        let (base, requests) = crate::testsupport::spawn_mock_embeddings(1).await;
        let client = without_proxy(EmbeddingsClient::with_retry(&cfg(4), &base, fast_retry(3)));
        let out = client
            .request_with_retry(&["match".to_string()])
            .await
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], vec![1.0, 0.0, 0.0, 0.0]);
        assert_eq!(requests.load(Ordering::SeqCst), 2, "the 429 must be retried");
    }

    #[tokio::test]
    async fn request_with_retry_gives_up_after_max_retries() {
        let (base, requests) = crate::testsupport::spawn_mock_embeddings(999).await;
        let client = without_proxy(EmbeddingsClient::with_retry(&cfg(4), &base, fast_retry(2)));
        let err = client
            .request_with_retry(&["x".to_string()])
            .await
            .unwrap_err();
        match err {
            EmbedError::Http { status, .. } => assert_eq!(status, 429),
            other => panic!("unexpected error: {other:?}"),
        }
        assert_eq!(
            requests.load(Ordering::SeqCst),
            3,
            "one attempt plus two retries"
        );
    }

    #[tokio::test]
    async fn request_with_retry_does_not_retry_a_client_error() {
        let (base, requests) = crate::testsupport::spawn_mock_embeddings_error(400, "nope").await;
        let client = without_proxy(EmbeddingsClient::with_retry(&cfg(4), &base, fast_retry(3)));
        let err = client
            .request_with_retry(&["x".to_string()])
            .await
            .unwrap_err();
        assert!(!err.is_retryable());
        assert_eq!(requests.load(Ordering::SeqCst), 1, "no retry on 4xx");
    }

    // --- single request ---

    #[tokio::test]
    async fn one_request_rejects_a_mismatched_item_count() {
        let base = spawn_payload(serde_json::json!({"data": [
            {"index": 0, "embedding": [1.0, 0.0]},
            {"index": 1, "embedding": [0.0, 1.0]}
        ]}))
        .await;
        let client = without_proxy(EmbeddingsClient::with_retry(&cfg(4), &base, fast_retry(0)));
        let err = client.one_request(&["only one".to_string()]).await.unwrap_err();
        let msg = err.to_string();
        match err {
            EmbedError::Count { want, got } => {
                assert_eq!(want, 1);
                assert_eq!(got, 2);
            }
            other => panic!("unexpected error: {other:?}"),
        }
        assert!(msg.contains("响应条数不符"), "err: {msg}");
    }

    #[tokio::test]
    async fn one_request_rejects_inconsistent_dimensions() {
        let base = spawn_payload(serde_json::json!({"data": [
            {"index": 0, "embedding": [1.0, 0.0, 0.0, 0.0]},
            {"index": 1, "embedding": [0.0, 1.0, 0.0]}
        ]}))
        .await;
        let client = without_proxy(EmbeddingsClient::with_retry(&cfg(4), &base, fast_retry(0)));
        let err = client
            .one_request(&["a".to_string(), "b".to_string()])
            .await
            .unwrap_err();
        let msg = err.to_string();
        match err {
            EmbedError::Dim { first, idx, got } => {
                assert_eq!(first, 4);
                assert_eq!(idx, 1);
                assert_eq!(got, 3);
            }
            other => panic!("unexpected error: {other:?}"),
        }
        assert!(msg.contains("维度不一致"), "err: {msg}");
    }

    #[tokio::test]
    async fn one_request_surfaces_the_server_error_body() {
        let (base, _req) =
            crate::testsupport::spawn_mock_embeddings_error(400, "input limit exceeded: max 10")
                .await;
        let client = without_proxy(EmbeddingsClient::with_retry(&cfg(4), &base, fast_retry(0)));
        let err = client.one_request(&["x".to_string()]).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("400"), "status missing from: {msg}");
        assert!(
            msg.contains("input limit exceeded"),
            "server detail missing from: {msg}"
        );
    }

    #[tokio::test]
    async fn one_request_maps_a_transport_failure_to_a_network_error() {
        let port = closed_port().await;
        let c = cfg(1);
        let client = without_proxy(EmbeddingsClient::with_retry(
            &c,
            &format!("http://127.0.0.1:{port}/v1"),
            fast_retry(0),
        ));
        let err = client.one_request(&["x".to_string()]).await.unwrap_err();
        assert!(
            matches!(&err, EmbedError::Network(_)),
            "expected a network error, got {err:?}"
        );
    }

    // --- jitter / truncation helpers ---

    #[test]
    fn rand_jitter_stays_inside_its_bounded_span() {
        for base in [0u64, 1, 4, 100, 1000] {
            for _ in 0..50 {
                let j = rand_jitter(base);
                assert!(j >= base / 4, "jitter {j} below the lower bound for {base}");
                assert!(
                    j <= base / 4 + base / 4,
                    "jitter {j} above the upper bound for {base}"
                );
            }
        }
    }

    #[test]
    fn truncate_chars_keeps_short_input_untouched() {
        assert_eq!(truncate_chars("hello", 10), "hello");
        assert_eq!(truncate_chars("hello", 5), "hello", "exactly at the limit");
        assert_eq!(truncate_chars("", 10), "");
    }

    #[test]
    fn truncate_chars_marks_truncated_input_with_an_ellipsis() {
        let out = truncate_chars("abcdef", 3);
        assert_eq!(out, "abc…");
        assert_eq!(out.chars().count(), 4);
    }

    #[test]
    fn truncate_chars_is_char_boundary_safe() {
        let out = truncate_chars("你好世界", 2);
        assert_eq!(out, "你好…");
    }

    #[test]
    fn truncate_chars_with_a_zero_limit_keeps_only_the_ellipsis() {
        assert_eq!(truncate_chars("abc", 0), "…");
    }
}
