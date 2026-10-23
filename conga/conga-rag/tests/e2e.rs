//! End-to-end smoke: fixtures on disk → mock embeddings → ingest ×2 → search.

use conga_rag::config::{EmbeddingConfig, RagConfig, SourceConfig};
use conga_rag::pipeline::run_ingest;
use conga_rag::search::run_search;

fn write_cfg(dir: &std::path::Path, db: &std::path::Path, base_url: &str) -> RagConfig {
    let mut sources = std::collections::BTreeMap::new();
    sources.insert(
        "notes".to_string(),
        SourceConfig {
            kind: "dir".into(),
            path: dir.to_path_buf(),
            include: vec!["**/*.md".into()],
            exclude: vec![],
        },
    );
    RagConfig {
        sources,
        embedding: EmbeddingConfig {
            base_url: Some(base_url.into()),
            api_key: Some("k".into()),
            model: Some("mock".into()),
            batch: 4,
            min_interval_ms: 0,
        },
        ..Default::default()
    }
    .with_store_path(db.to_path_buf())
}

#[tokio::test]
async fn end_to_end_ingest_search() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hit.md"), "# Doc\n\nmatch the answer here").unwrap();
    std::fs::write(dir.path().join("miss.md"), "完全无关的内容").unwrap();
    let (base, _req) = conga_rag::testsupport::spawn_mock_embeddings(0).await;
    let dbdir = tempfile::tempdir().unwrap();
    let cfg = write_cfg(dir.path(), &dbdir.path().join("e2e.db"), &base);

    let s1 = run_ingest(&cfg, None, false).await.unwrap();
    assert_eq!((s1.added, s1.failed), (2, 0));
    let s2 = run_ingest(&cfg, None, false).await.unwrap();
    assert_eq!((s2.added, s2.updated, s2.removed), (0, 0, 0), "idempotent");

    let hits = run_search(&cfg, "match", 2, None).await.unwrap();
    assert!(!hits.is_empty());
    assert!(
        hits[0].content.contains("match"),
        "top hit must be the match doc"
    );
}

// --- CLI level (main / load_config_or_exit / print_stats) ---

/// Run the built CLI in a hermetic environment: `HOME` and the working
/// directory point into the temp dir (no `~/.conga/config.toml`, no stray
/// `.env`), and `NO_PROXY` keeps the local mock server out of the proxy.
fn run_cli(dir: &std::path::Path, args: &[&str], env: &[(&str, &str)]) -> std::process::Output {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_conga-rag"));
    cmd.current_dir(dir)
        .env("HOME", dir.join("home"))
        .env("NO_PROXY", "127.0.0.1,localhost")
        .env("no_proxy", "127.0.0.1,localhost")
        .env_remove("CONGA_RAG_CONFIG")
        .env_remove("CONGA_RAG_BUILTIN_BASE")
        .env_remove("CONGA_CONFIG")
        .env_remove("CONGA_LLM_BASE_URL")
        .env_remove("CONGA_LLM_KEY")
        .args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("cli process")
}

/// Mock embeddings server on its own runtime: the CLI child process must
/// reach it while the test blocks on the child's output, so the server must
/// not live on the test's runtime.
fn mock_embeddings_server() -> (String, tokio::runtime::Runtime) {
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let base = rt.block_on(async { conga_rag::testsupport::spawn_mock_embeddings(0).await.0 });
    (base, rt)
}

/// A `rag.toml` with one source, the mock embeddings endpoint and an
/// explicit store path.
fn write_cli_rag(
    dir: &std::path::Path,
    src: &std::path::Path,
    db: &std::path::Path,
    base: &str,
) -> std::path::PathBuf {
    let rag = dir.join("rag.toml");
    std::fs::write(
        &rag,
        format!(
            "[sources.docs]\ntype = \"dir\"\npath = \"{}\"\ninclude = [\"**/*.md\"]\n\n\
             [embedding]\nbase_url = \"{}\"\napi_key = \"k\"\nmodel = \"mock\"\n\n\
             [store]\npath = \"{}\"\n",
            src.display(),
            base,
            db.display()
        ),
    )
    .unwrap();
    rag
}

#[test]
fn cli_ingest_prints_the_stats_as_json() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("home")).unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("hit.md"), "# Doc\n\nmatch the answer").unwrap();
    let (base, _server) = mock_embeddings_server();
    let db = dir.path().join("index.db");
    let rag = write_cli_rag(dir.path(), &src, &db, &base);

    let out = run_cli(
        dir.path(),
        &["--json", "ingest"],
        &[("CONGA_RAG_CONFIG", rag.to_str().unwrap())],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "cli must succeed: {stdout}\n{stderr}");
    let line = stdout.lines().last().expect("a stats line on stdout");
    let stats: serde_json::Value = serde_json::from_str(line).expect("stats must be JSON");
    // KNOWN BUG (src/main.rs:71-78): `print_stats` is handed the *config
    // file* path and publishes it under the key `store`, while `status
    // --json` publishes `cfg.store_path()` under the very same key. The
    // assertion pins today's behaviour; the fix belongs to the production
    // code (see the test report).
    assert_eq!(stats["store"], rag.to_string_lossy().as_ref());
    assert_eq!(stats["added"], 1);
    assert_eq!(stats["failed"], 0);
    assert!(stats["chunks"].as_u64().unwrap() >= 1, "stats: {stats}");
}

#[test]
fn cli_ingest_prints_human_readable_stats() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("home")).unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("hit.md"), "match the answer").unwrap();
    let (base, _server) = mock_embeddings_server();
    let db = dir.path().join("index.db");
    let rag = write_cli_rag(dir.path(), &src, &db, &base);

    let out = run_cli(
        dir.path(),
        &["ingest"],
        &[("CONGA_RAG_CONFIG", rag.to_str().unwrap())],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "cli must succeed: {stdout}");
    assert!(
        stdout.contains("scanned=1 added=1"),
        "the text stats line must report the counters: {stdout}"
    );
}

#[test]
fn cli_status_reports_the_index() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("home")).unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    let (base, _server) = mock_embeddings_server();
    let db = dir.path().join("index.db");
    let rag = write_cli_rag(dir.path(), &src, &db, &base);

    let out = run_cli(
        dir.path(),
        &["--json", "status"],
        &[("CONGA_RAG_CONFIG", rag.to_str().unwrap())],
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "cli must succeed: {stdout}\n{stderr}");
    let line = stdout.lines().last().expect("a status line on stdout");
    let status: serde_json::Value = serde_json::from_str(line).expect("status must be JSON");
    assert_eq!(status["store"], db.to_string_lossy().as_ref());
    assert_eq!(status["sources"][0], "docs");
    assert!(
        status["fingerprint"].is_null(),
        "an empty index has no fingerprint"
    );
}

#[test]
fn cli_reports_a_missing_config_with_exit_code_2() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("home")).unwrap();
    let missing = dir.path().join("no-such-rag.toml");

    let out = run_cli(
        dir.path(),
        &["--json", "status"],
        &[("CONGA_RAG_CONFIG", missing.to_str().unwrap())],
    );
    assert_eq!(out.status.code(), Some(2), "a config error exits with 2");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("CONGA_RAG_CONFIG"),
        "the error must name the variable: {stderr}"
    );
}
