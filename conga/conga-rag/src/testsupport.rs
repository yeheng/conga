//! Shared test doubles (unit tests + doc examples). Compiled always; tiny.

#![allow(dead_code)]

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::Value;

/// Deterministic embeddings mock: every text gets a 4-dim vector derived from
/// its length; `[1,0,0,0]` for texts containing the word "match" so retrieval
/// tests are predictable. Records request count; fails the first N requests
/// with 429 when `fail_first` > 0.
pub async fn spawn_mock_embeddings(fail_first: u32) -> (String, Arc<AtomicU32>) {
    let state = Arc::new(AtomicU32::new(fail_first));
    let requests = Arc::new(AtomicU32::new(0));
    let req_clone = requests.clone();
    let fail_clone = state.clone();

    let app = Router::new().route(
        "/v1/embeddings",
        post(move |Json(body): Json<Value>| {
            let req_clone = req_clone.clone();
            let fail_clone = fail_clone.clone();
            async move {
                req_clone.fetch_add(1, Ordering::SeqCst);
                if fail_clone.load(Ordering::SeqCst) > 0 {
                    fail_clone.fetch_sub(1, Ordering::SeqCst);
                    return (StatusCode::TOO_MANY_REQUESTS, "rate limited").into_response();
                }
                let texts = body["input"].as_array().unwrap();
                let data: Vec<Value> = texts
                    .iter()
                    .enumerate()
                    .map(|(i, t)| {
                        let s = t.as_str().unwrap_or("");
                        let v = if s.contains("match") {
                            vec![1.0f64, 0.0, 0.0, 0.0]
                        } else {
                            vec![0.0f64, (s.len() as f64) / 1000.0, 0.0, 0.0]
                        };
                        serde_json::json!({"index": i, "embedding": v})
                    })
                    .collect();
                (StatusCode::OK, Json(serde_json::json!({"data": data}))).into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/v1"), requests)
}

/// Error-path double: every request answers `status` with `body`. Use to
/// assert the client surfaces server error details verbatim.
pub async fn spawn_mock_embeddings_error(
    status: u16,
    body: &'static str,
) -> (String, Arc<AtomicU32>) {
    let requests = Arc::new(AtomicU32::new(0));
    let req_clone = requests.clone();
    let status = StatusCode::from_u16(status).unwrap();

    let app = Router::new().route(
        "/v1/embeddings",
        post(move || {
            let req_clone = req_clone.clone();
            async move {
                req_clone.fetch_add(1, Ordering::SeqCst);
                (status, body).into_response()
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/v1"), requests)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Proxy-free client: the machine-wide `HTTP_PROXY` would otherwise
    /// hijack requests aimed at the local mock server.
    fn client() -> reqwest::Client {
        reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .expect("proxy-free client")
    }

    async fn post(base: &str, input: serde_json::Value) -> reqwest::Response {
        client()
            .post(format!("{base}/embeddings"))
            .json(&serde_json::json!({"model": "mock", "input": input}))
            .send()
            .await
            .expect("mock request")
    }

    #[tokio::test]
    async fn mock_embeddings_returns_deterministic_vectors() {
        let (base, requests) = spawn_mock_embeddings(0).await;
        let resp = post(&base, serde_json::json!(["match", "abc"])).await;
        assert_eq!(resp.status(), 200);
        let body: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(body["data"][0]["index"], 0);
        assert_eq!(
            body["data"][0]["embedding"],
            serde_json::json!([1.0, 0.0, 0.0, 0.0]),
            "\"match\" must map to the retrieval vector"
        );
        assert_eq!(
            body["data"][1]["embedding"],
            serde_json::json!([0.0, 0.003, 0.0, 0.0]),
            "other texts map to their length"
        );
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn mock_embeddings_fails_the_first_requests_with_429() {
        let (base, requests) = spawn_mock_embeddings(1).await;
        let first = post(&base, serde_json::json!(["x"])).await;
        assert_eq!(first.status(), 429);
        assert_eq!(first.text().await.unwrap(), "rate limited");
        let second = post(&base, serde_json::json!(["x"])).await;
        assert_eq!(second.status(), 200, "the failure budget is spent");
        assert_eq!(requests.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn mock_embeddings_error_answers_with_the_given_status_and_body() {
        let (base, requests) = spawn_mock_embeddings_error(503, "upstream is down").await;
        let resp = post(&base, serde_json::json!(["x"])).await;
        assert_eq!(resp.status(), 503);
        assert_eq!(resp.text().await.unwrap(), "upstream is down");
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }
}
