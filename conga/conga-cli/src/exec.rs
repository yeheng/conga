//! `conga exec` — headless one-shot runner for CI and scripts.
//!
//! One turn, no REPL: the SAME host wiring as the REPL
//! (`SessionAssembly::build_cli`), one `run_turn`, the outcome mapped to an
//! exit code. `--json` emits the same wire schema as the gateway/desktop
//! (`event_map::event_to_ws` → `OutgoingEvent`), one NDJSON line per event;
//! human mode reuses the REPL's `EventPrinter` on stdout. All status chatter
//! (session id, approval denials, subagent taps) goes to stderr so stdout
//! stays machine-parseable in `--json` mode.

use std::io::{Read, Write};
use std::sync::Arc;

use conga::{AgentEvent, TurnEndReason};
use conga_host::Mode;

/// Parsed `conga exec` arguments (everything after the `exec` word).
pub(crate) struct ExecArgs {
    /// The task text; `-` means read it from stdin (resolved in `run`).
    pub(crate) task: Option<String>,
    pub(crate) json: bool,
    /// Default `FullAuto`: exec has no human to answer an approval prompt,
    /// so anything stricter neuters the run (the headless approver denies).
    pub(crate) mode: Mode,
    pub(crate) resume: Option<String>,
    /// JS extension scripts (`--ext-js=<path>`, repeatable).
    pub(crate) ext_js: Vec<std::path::PathBuf>,
}

/// Usage line, shared by every error path so the hints cannot drift.
pub(crate) const USAGE: &str = "usage: conga exec [--json] [--mode=<suggest|auto-edit|full-auto|plan>] [--resume=<id|last>] [--ext-js=<script.js>] <task|->";

pub(crate) fn parse_args(args: &[String]) -> Result<ExecArgs, String> {
    let mut out = ExecArgs {
        task: None,
        json: false,
        mode: Mode::FullAuto,
        resume: None,
        ext_js: Vec::new(),
    };
    for a in args {
        if let Some(v) = a.strip_prefix("--mode=") {
            out.mode = Mode::parse(v).ok_or_else(|| format!("unknown --mode: {v}\n{USAGE}"))?;
        } else if let Some(v) = a.strip_prefix("--resume=") {
            out.resume = Some(v.to_string());
        } else if let Some(v) = a.strip_prefix("--ext-js=") {
            out.ext_js.push(std::path::PathBuf::from(v));
        } else if a == "--json" {
            out.json = true;
        } else if a.starts_with("--") {
            return Err(format!("unknown flag: {a}\n{USAGE}"));
        } else if out.task.is_none() {
            out.task = Some(a.clone());
        } else {
            return Err(format!("unexpected extra argument: {a}\n{USAGE}"));
        }
    }
    Ok(out)
}

/// Exit-code contract for CI:
/// 0 = completed · 1 = turn errored · 130 = aborted (SIGINT convention).
pub(crate) fn exit_code(reason: &TurnEndReason) -> i32 {
    match reason {
        TurnEndReason::Completed => 0,
        TurnEndReason::Error { .. } => 1,
        TurnEndReason::Aborted { .. } => 130,
    }
}

/// Build the host (REPL wiring), run ONE turn, map the outcome to an exit
/// code. Returns 2 for usage/setup errors (distinct from a turn error).
pub(crate) async fn run(args: &[String]) -> i32 {
    let parsed = match parse_args(args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    let task = match parsed.task.as_deref() {
        Some("-") => {
            let mut buf = String::new();
            if std::io::stdin().read_to_string(&mut buf).is_err() {
                eprintln!("failed to read task from stdin");
                return 2;
            }
            buf
        }
        Some(t) => t.to_string(),
        None => {
            eprintln!("{USAGE}");
            return 2;
        }
    };

    // Headless approver: nobody is watching, so a consent prompt is a deny
    // (surfaced on stderr; FullAuto/Plan never reach it anyway).
    let deny: conga_host::permission::Approver = Arc::new(|name: &str, _: &serde_json::Value| {
        let name = name.to_string();
        Box::pin(async move {
            eprintln!("[exec] approval for {name} denied (headless)");
            false
        })
    });
    let (ext_tools, ext_hooks) = crate::load_inprocess_ext();
    let (js_tools, js_hooks) = crate::load_js_ext_for(&parsed.ext_js);
    let all_tools: Vec<conga::ToolDefinition> =
        ext_tools.iter().chain(js_tools.iter()).cloned().collect();
    let all_hooks: Vec<Arc<dyn conga::HookChain>> = ext_hooks.into_iter().chain(js_hooks).collect();
    let host = match conga_host::SessionAssembly::build_cli(
        parsed.mode,
        deny,
        parsed.resume,
        all_hooks,
        all_tools,
    )
    .await
    {
        Ok(h) => h,
        Err(e) => {
            eprintln!("{e}");
            return 2;
        }
    };
    eprintln!("[exec] session {}", host.session().current_id());
    conga_host::install_ctrl_c(host.signal().clone());

    let mut tool_names = std::collections::HashMap::new();
    // Cumulative usage for the terminal done line — same schema and
    // semantics as the gateway's turn-boundary emission (ws.rs WireEvent::Done):
    // accumulate provider-reported usage, emit done (with summary when the
    // turn actually elapsed) after the last event, then the error line on
    // failure. (in, out, cache_read, cache_write).
    let usage = Arc::new((
        std::sync::atomic::AtomicU64::new(0),
        std::sync::atomic::AtomicU64::new(0),
        std::sync::atomic::AtomicU64::new(0),
        std::sync::atomic::AtomicU64::new(0),
    ));
    let usage_c = usage.clone();
    let turn_start = std::time::Instant::now();
    let mut printer = conga_host::EventPrinter::new(std::io::stdout());
    let json = parsed.json;
    let summary = host
        .run_turn(&task, move |ev| {
            if !json {
                printer.on_event(&ev);
                return;
            }
            if let AgentEvent::AfterProviderResponse { response, .. } = &ev {
                if let Some(u) = &response.usage {
                    use std::sync::atomic::Ordering::Relaxed;
                    usage_c.0.fetch_add(u.input_tokens, Relaxed);
                    usage_c.1.fetch_add(u.output_tokens, Relaxed);
                    usage_c.2.fetch_add(u.cache_read_tokens.unwrap_or(0), Relaxed);
                    usage_c.3.fetch_add(u.cache_write_tokens.unwrap_or(0), Relaxed);
                }
            }
            if let Some(ws) = conga_host::event_map::event_to_ws(&ev, &mut tool_names) {
                let mut lock = std::io::stdout().lock();
                let _ = writeln!(lock, "{}", serde_json::to_string(&ws).unwrap_or_default());
            }
        })
        .await;
    if json {
        use std::sync::atomic::Ordering::Relaxed;
        let elapsed_ms = turn_start.elapsed().as_millis() as u64;
        let done = if elapsed_ms > 0 {
            conga_host::wire::OutgoingEvent::done_with_summary(
                usage.0.load(Relaxed),
                usage.1.load(Relaxed),
                usage.2.load(Relaxed),
                usage.3.load(Relaxed),
                elapsed_ms,
            )
        } else {
            conga_host::wire::OutgoingEvent::done()
        };
        let mut lock = std::io::stdout().lock();
        let _ = writeln!(lock, "{}", serde_json::to_string(&done).unwrap_or_default());
        if let Err(e) = &summary {
            let ev = conga_host::wire::OutgoingEvent::error(format!("{e}"));
            let _ = writeln!(lock, "{}", serde_json::to_string(&ev).unwrap_or_default());
        }
    }
    let _ = std::io::stdout().flush();

    match summary {
        Ok(s) => exit_code(&s.reason),
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn parse_task_and_flags() {
        let a = parse_args(&s(&[
            "--json",
            "--mode=plan",
            "--resume=last",
            "fix the bug",
        ]))
        .expect("parse");
        assert_eq!(a.task.as_deref(), Some("fix the bug"));
        assert!(a.json);
        assert_eq!(a.mode, Mode::Plan);
        assert_eq!(a.resume.as_deref(), Some("last"));
    }

    #[test]
    fn parse_defaults_full_auto_fresh_session() {
        let a = parse_args(&s(&["do things"])).expect("parse");
        assert_eq!(a.task.as_deref(), Some("do things"));
        assert!(!a.json);
        assert_eq!(a.mode, Mode::FullAuto);
        assert!(a.resume.is_none());
    }

    #[test]
    fn parse_rejects_unknown_flag_and_extra_positional_and_bad_mode() {
        assert!(parse_args(&s(&["--wat", "x"])).is_err());
        assert!(parse_args(&s(&["one", "two"])).is_err());
        assert!(parse_args(&s(&["--mode=bogus", "x"])).is_err());
    }

    #[test]
    fn parse_missing_task_is_ok_placeholder_checked_in_run() {
        // `run` decides (stdin `-`, usage error); parse only collects.
        let a = parse_args(&s(&["--json"])).expect("parse");
        assert!(a.task.is_none());
    }

    #[test]
    fn exit_codes_follow_ciconvention() {
        assert_eq!(exit_code(&TurnEndReason::Completed), 0);
        assert_eq!(
            exit_code(&TurnEndReason::Error {
                message: "boom".into()
            }),
            1
        );
        assert_eq!(
            exit_code(&TurnEndReason::Aborted {
                cause: Some(conga::CancelCause::User)
            }),
            130
        );
    }
}
