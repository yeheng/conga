//! JSON-RPC daemon runner — long-lived script process with request multiplexing.
//!
//! Replaces the one-shot `run_jsonrpc` with a persistent process that handles
//! multiple requests over the same stdin/stdout pipe, eliminating cold-start
//! overhead on every tool invocation.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_stream::StreamExt;
use tokio_util::codec::{FramedRead, LinesCodec};

use crate::external_tools::dispatcher::{DispatcherContext, RpcDispatcher};
use crate::external_tools::manifest::{Permission, PluginManifest};
use crate::external_tools::rpc::{
    decode, encode, RpcError, RpcMessage, RpcRequest, RpcResponse, MAX_MESSAGE_SIZE,
};
use crate::external_tools::runner::simple::spawn_process;
use crate::external_tools::runner::{PluginError, PluginResult};

/// Persistent JSON-RPC script process.
pub struct JsonRpcDaemon {
    pending: Arc<DashMap<i64, oneshot::Sender<RpcResponse>>>,
    write_tx: mpsc::UnboundedSender<RpcMessage>,
    next_id: AtomicI64,
    last_used_ms: AtomicI64,
    /// Max wait for a single `call()` round-trip.
    call_timeout_ms: i64,
    /// Daemon is considered idle-expired (and replaced) after this many ms.
    idle_timeout_ms: i64,
    stderr: Arc<tokio::sync::Mutex<String>>,
    alive: Arc<AtomicBool>,
    child: Arc<tokio::sync::Mutex<tokio::process::Child>>,
    /// Background task handles for graceful shutdown.
    tasks: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// Shutdown signal for background tasks.
    shutdown_tx: watch::Sender<bool>,
}

impl JsonRpcDaemon {
    /// Spawn a new persistent JSON-RPC process.
    ///
    /// `call_timeout_secs` bounds a single `call()` round-trip;
    /// `idle_timeout_secs` bounds how long the daemon stays alive between
    /// calls before being considered expired by `is_idle_expired()`.
    pub async fn spawn(
        manifest: &PluginManifest,
        manifest_dir: &Path,
        call_timeout_secs: u64,
        idle_timeout_secs: u64,
        permissions: &[Permission],
        dispatcher: &RpcDispatcher,
        ctx: &DispatcherContext,
    ) -> Result<Self, PluginError> {
        let mut child = spawn_process(manifest, manifest_dir)?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| PluginError::Io("Failed to open stdin".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| PluginError::Io("Failed to open stdout".to_string()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| PluginError::Io("Failed to open stderr".to_string()))?;

        let pending: Arc<DashMap<i64, oneshot::Sender<RpcResponse>>> = Arc::new(DashMap::new());
        let (write_tx, mut write_rx) = mpsc::unbounded_channel::<RpcMessage>();
        let alive = Arc::new(AtomicBool::new(true));
        let child = Arc::new(tokio::sync::Mutex::new(child));
        let (shutdown_tx, _) = watch::channel(false);

        // ── Writer task ──────────────────────────────────────────
        let mut shutdown_rx_w = shutdown_tx.subscribe();
        let mut stdin = stdin;
        let writer_handle = tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(msg) = write_rx.recv() => {
                        let encoded = encode(&msg);
                        if stdin.write_all(encoded.as_bytes()).await.is_err() {
                            break;
                        }
                        if stdin.flush().await.is_err() {
                            break;
                        }
                    }
                    _ = shutdown_rx_w.changed() => break,
                }
            }
        });

        // ── Stderr collector ─────────────────────────────────────
        let stderr_buffer = Arc::new(tokio::sync::Mutex::new(String::new()));
        let stderr_buffer_clone = stderr_buffer.clone();
        let mut shutdown_rx_e = shutdown_tx.subscribe();
        let stderr_handle = tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            loop {
                tokio::select! {
                    result = reader.read_line(&mut line) => {
                        match result {
                            Ok(0) => break,
                            Ok(_) => {
                                stderr_buffer_clone.lock().await.push_str(&line);
                                line.clear();
                            }
                            Err(_) => break,
                        }
                    }
                    _ = shutdown_rx_e.changed() => break,
                }
            }
        });

        // ── Reader task ──────────────────────────────────────────
        let pending_reader = pending.clone();
        let permissions = permissions.to_vec();
        let dispatcher = dispatcher.clone();
        let ctx = ctx.clone();
        let write_tx_clone = write_tx.clone();
        let alive_reader = alive.clone();
        let child_reader = child.clone();
        let mut shutdown_rx_r = shutdown_tx.subscribe();
        let reader_handle = tokio::spawn(async move {
            let mut reader_lines =
                FramedRead::new(stdout, LinesCodec::new_with_max_length(MAX_MESSAGE_SIZE));
            loop {
                tokio::select! {
                    result = reader_lines.next() => {
                        match result {
                            Some(Ok(line)) => {
                                let msg = match decode(&line) {
                                    None => continue,
                                    Some(Ok(m)) => m,
                                    Some(Err(e)) => {
                                        tracing::error!(
                                            "[daemon] Plugin emitted invalid JSON-RPC output, killing process: {} — line: {:.100}",
                                            e, line
                                        );
                                        // Fail all pending requests
                                        let keys: Vec<i64> = pending_reader.iter().map(|entry| *entry.key()).collect();
                                        for key in keys {
                                            if let Some((_, tx)) = pending_reader.remove(&key) {
                                                let _ = tx.send(RpcResponse {
                                                    jsonrpc: "2.0".to_string(),
                                                    id: serde_json::json!(key),
                                                    result: None,
                                                    error: Some(RpcError {
                                                        code: -32700,
                                                        message: format!("Plugin emitted invalid output: {}", e),
                                                        data: None,
                                                    }),
                                                });
                                            }
                                        }
                                        alive_reader.store(false, Ordering::Relaxed);
                                        break;
                                    }
                                };
                                match msg {
                                    RpcMessage::Request(request) => {
                                        let response =
                                            dispatcher.dispatch(request, &permissions, &ctx).await;
                                        let _ = write_tx_clone.send(RpcMessage::Response(response));
                                    }
                                    RpcMessage::Response(response) => {
                                        if let Some(id) = response.id.as_i64() {
                                            if let Some((_, tx)) = pending_reader.remove(&id) {
                                                let _ = tx.send(response);
                                            }
                                        }
                                    }
                                }
                            }
                            Some(Err(_)) | None => break,
                        }
                    }
                    _ = shutdown_rx_r.changed() => break,
                }
            }
            alive_reader.store(false, Ordering::Relaxed);
            // stdout closed — process is dead or dying
            let _ = child_reader.lock().await.kill().await;
        });

        let tasks = vec![writer_handle, stderr_handle, reader_handle];

        Ok(Self {
            pending,
            write_tx,
            next_id: AtomicI64::new(1),
            last_used_ms: AtomicI64::new(chrono::Utc::now().timestamp_millis()),
            call_timeout_ms: (call_timeout_secs as i64) * 1000,
            idle_timeout_ms: (idle_timeout_secs as i64) * 1000,
            stderr: stderr_buffer,
            alive,
            child,
            tasks: std::sync::Mutex::new(tasks),
            shutdown_tx,
        })
    }

    /// Execute a single JSON-RPC call through the persistent process.
    pub async fn call(
        &self,
        method: &str,
        params: Option<Value>,
    ) -> Result<PluginResult, PluginError> {
        if !self.alive.load(Ordering::Relaxed) {
            return Err(PluginError::Io(
                "JSON-RPC daemon process has exited".to_string(),
            ));
        }

        // Every call gets a fresh, monotonic ID. Using a magic id=0 for
        // `initialize` causes DashMap key collisions when callers issue
        // concurrent initializes — the second insert silently overwrites the
        // first oneshot::Sender, dooming the first caller to wait until
        // timeout.
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.insert(id, tx);

        let request = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(Value::Number(id.into())),
            method: method.to_string(),
            params,
        };
        self.write_tx
            .send(RpcMessage::Request(request))
            .map_err(|_| PluginError::Io("Daemon write channel closed".to_string()))?;

        self.touch();

        let timeout_duration = Duration::from_millis(self.call_timeout_ms.max(5000) as u64);
        let response = match tokio::time::timeout(timeout_duration, rx).await {
            Ok(Ok(resp)) => resp,
            Ok(Err(_)) => {
                self.pending.remove(&id);
                return Err(PluginError::Io(
                    "Daemon response channel closed".to_string(),
                ));
            }
            Err(_) => {
                self.pending.remove(&id);
                return Err(PluginError::Timeout(
                    self.call_timeout_ms.max(5000) as u64 / 1000,
                ));
            }
        };

        if let Some(error) = response.error {
            return Err(PluginError::InvalidOutput(format!(
                "JSON-RPC error: {} (code {})",
                error.message, error.code
            )));
        }

        let output = response.result.ok_or_else(|| {
            PluginError::InvalidOutput("No result received from daemon".to_string())
        })?;

        let stderr = self.stderr.lock().await.clone();

        Ok(PluginResult {
            output,
            stderr,
            duration: Duration::default(),
        })
    }

    /// Run a one-shot JSON-RPC call, spawning and shutting down the daemon
    /// for a single invocation.
    pub async fn run_once(
        manifest: &PluginManifest,
        manifest_dir: &Path,
        args: &Value,
        timeout_secs: u64,
        permissions: &[Permission],
        dispatcher: &RpcDispatcher,
        ctx: &DispatcherContext,
    ) -> Result<PluginResult, PluginError> {
        let daemon = Self::spawn(
            manifest,
            manifest_dir,
            timeout_secs,
            timeout_secs,
            permissions,
            dispatcher,
            ctx,
        )
        .await?;
        let result = daemon.call("initialize", Some(args.clone())).await;
        daemon.shutdown().await;
        result
    }

    /// Gracefully shut down the daemon and kill the underlying process.
    pub async fn shutdown(&self) {
        self.alive.store(false, Ordering::Relaxed);
        let _ = self.shutdown_tx.send(true);
        let tasks: Vec<_> = self
            .tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain(..)
            .collect();
        for handle in tasks {
            let _ = handle.await;
        }
        let _ = self.child.lock().await.kill().await;
    }

    /// Returns true if the daemon has been idle longer than its timeout.
    pub fn is_idle_expired(&self) -> bool {
        let now = chrono::Utc::now().timestamp_millis();
        let last = self.last_used_ms.load(Ordering::Relaxed);
        now - last > self.idle_timeout_ms
    }

    fn touch(&self) {
        self.last_used_ms
            .store(chrono::Utc::now().timestamp_millis(), Ordering::Relaxed);
    }
}

impl Drop for JsonRpcDaemon {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Relaxed);
        let _ = self.shutdown_tx.send(true);
        // Best-effort async cleanup: spawn a task to kill the child.
        // Tasks will exit naturally once the child process dies and
        // pipes close, but we expedite it here.
        let child = self.child.clone();
        tokio::spawn(async move {
            let _ = child.lock().await.kill().await;
        });
    }
}
