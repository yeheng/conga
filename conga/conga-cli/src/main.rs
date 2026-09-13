//! conga CLI REPL: 持一个 Host，每行调一次 run_turn，交互式终端 agent。
//! `conga exec <task>`（见 `exec.rs`）是无头一次性入口，共用同一装配。
use std::future::Future;
use std::io::{self, Write};
use std::pin::Pin;
use std::sync::Arc;

use conga::ToolDefinition;
use conga_host::{gather_tools, install_ctrl_c, EventPrinter, Host, Mode, SessionAssembly};
use reedline::{DefaultPrompt, Reedline, Signal};
mod exec;

/// In-process extensions behind feature `ext`: tools + optional hook chain
/// (`permission_gate`). Without the feature, empty tools / no extra hooks.
fn load_inprocess_ext() -> (Vec<ToolDefinition>, Option<Arc<dyn conga::HookChain>>) {
    #[cfg(feature = "ext")]
    {
        use conga::ExtensionApiImpl;
        let mut api = ExtensionApiImpl::new();
        conga_ext::register_all(&mut api);
        let tools = std::mem::take(&mut api.tools);
        let hooks: Arc<dyn conga::HookChain> = Arc::new(api);
        (tools, Some(hooks))
    }
    #[cfg(not(feature = "ext"))]
    (Vec::new(), None)
}

/// JS extension scripts (feature `ext-js`). Fails loud at startup: a script
/// that throws is a config error, not a soft warning.
fn load_js_ext_for(paths: &[std::path::PathBuf]) -> (Vec<ToolDefinition>, Vec<Arc<dyn conga::HookChain>>) {
    if paths.is_empty() {
        return (Vec::new(), Vec::new());
    }
    #[cfg(feature = "ext-js")]
    {
        match conga_host::ext_js::load_scripts(paths) {
            Ok(ext) => (
                ext.tools,
                // Always wrap the chain: an empty hook chain is a no-op.
                vec![ext.hooks],
            ),
            Err(e) => {
                eprintln!("ext-js error: {e}");
                std::process::exit(1);
            }
        }
    }
    #[cfg(not(feature = "ext-js"))]
    {
        let _ = paths;
        eprintln!("--ext-js requires a build with --features ext-js");
        std::process::exit(1);
    }
}

/// REPL entry: collect repeatable `--ext-js=<path>` from the process args.
fn load_js_ext() -> (Vec<ToolDefinition>, Vec<Arc<dyn conga::HookChain>>) {
    let paths: Vec<std::path::PathBuf> = std::env::args()
        .filter_map(|a| a.strip_prefix("--ext-js=").map(std::path::PathBuf::from))
        .collect();
    load_js_ext_for(&paths)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // config.toml base layer: file first, .env/env override. Must run before
    // any CONGA_* env read (both the exec path and the REPL below).
    if let Err(e) = conga::config_file::apply() {
        eprintln!("config error: {e}");
        std::process::exit(1);
    }
    // `conga exec ...` is the headless one-shot path; everything else is
    // the interactive REPL.
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(String::as_str) == Some("exec") {
        let code = exec::run(&argv[1..]).await;
        std::process::exit(code);
    }
    let mode = std::env::args()
        .find_map(|a| a.strip_prefix("--mode=").and_then(Mode::parse))
        .unwrap_or(Mode::AutoEdit);
    let resume_arg = std::env::args().find_map(|a| a.strip_prefix("--resume=").map(String::from));

    let (ext_tools, ext_hooks) = load_inprocess_ext();
    let (js_tools, js_hooks) = load_js_ext();
    let all_tools: Vec<ToolDefinition> = ext_tools.iter().chain(js_tools.iter()).cloned().collect();
    let all_hooks: Vec<Arc<dyn conga::HookChain>> =
        ext_hooks.into_iter().chain(js_hooks).collect();
    if !all_tools.is_empty() {
        eprintln!("(in-process ext tools: {})", all_tools.len());
    }

    // One shared assembly (same wiring as the gateway/desktop): skills
    // prompt, ext gate hooks before the permission policy, tool gathering,
    // sub-agent spawner, signal wiring. The stdin approver is the only
    // CLI-specific piece.
    let mut host = match SessionAssembly::build_cli(
        mode,
        Arc::new(stdin_approver),
        resume_arg,
        all_hooks,
        all_tools.clone(),
    )
    .await
    {
        Ok(h) => h,
        Err(e) => {
            eprintln!("config error: {e}\nset CONGA_LLM_* in .env or env");
            std::process::exit(1);
        }
    };

    // Cooperative-abort signal: a Ctrl-C during LLM streaming (cooked tty mode)
    // cancels and the agent loop exits at the next safe point, returning the
    // partial transcript. Every press is honored; run_turn resets the signal.
    // At the prompt (raw mode) Ctrl-C is a key event handled by reedline, not
    // a SIGINT, so it doesn't fire here.
    install_ctrl_c(host.signal().clone());

    let mut rl = Reedline::create();
    let prompt = DefaultPrompt::default();
    while let Ok(Signal::Success(line)) = rl.read_line(&prompt) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(cmd) = line.strip_prefix('/') {
            handle_slash(cmd, &mut host, &all_tools).await;
            continue;
        }
        // Working history (and its compaction) is log-derived inside
        // run_turn; JSONL on disk stays the append-only full log.
        let mut printer = EventPrinter::new(io::stdout());
        match host
            .run_turn(line, |ev| {
                printer.on_event(&ev);
            })
            .await
        {
            Ok(_summary) => {}
            Err(e) => eprintln!("\n(run error: {e})"),
        }
        let _ = io::stdout().flush();
    }
    Ok(())
}

/// Payload banner for approval prompts. Empty args (the no-payload
/// case) render nothing — the y/N prompt alone, unchanged UX. A payload
/// (evolve admission passes action/title/tags/body per candidate;
/// high-risk tool approvals pass their call args) renders as ONE JSON
/// line above the prompt: docs/evolve.md's content-quality gate requires
/// the approver to see what is being approved, and evolve candidates are
/// not tool calls, so nothing else would render them.
fn approval_banner(name: &str, args: &serde_json::Value) -> String {
    if args.as_object().is_none_or(|m| m.is_empty()) {
        return String::new();
    }
    let payload = serde_json::to_string(args).unwrap_or_default();
    format!("[approve {name}] {payload}\n")
}

fn stdin_approver<'a>(
    name: &'a str,
    args: &'a serde_json::Value,
) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>> {
    // 读 stdin 是阻塞的，挪到 blocking 池，避免卡住 tokio worker。
    // The banner is computed before the async block so the args borrow
    // ends here; an empty banner keeps the prompt byte-identical to the
    // old output.
    let banner = approval_banner(name, args);
    let name = name.to_string();
    Box::pin(async move {
        print!("{banner}\n[approve {name}? y/N] ");
        let _ = io::stdout().flush();
        tokio::task::spawn_blocking(move || {
            let mut s = String::new();
            let _ = io::stdin().read_line(&mut s);
            s.trim().eq_ignore_ascii_case("y")
        })
        .await
        .unwrap_or(false)
    })
}

async fn handle_slash(cmd: &str, host: &mut Host, all_tools: &[ToolDefinition]) {
    let mut parts = cmd.split_whitespace();
    match parts.next() {
        Some("exit") | Some("quit") => std::process::exit(0),
        Some("clear") => {
            // Unified semantics: append a Cleared fact to the log (id stays,
            // derive truncates, disk stays append-only). A failed write is
            // reported — a silent failure would resurrect the old history.
            match host.clear_session().await {
                Ok(()) => println!("(cleared)"),
                Err(e) => println!("(clear failed: {e})"),
            }
        }
        Some("mode") => match parts.next().and_then(Mode::parse) {
            Some(m) => {
                host.policy().set_mode(m);
                println!("(mode -> {m:?})");
            }
            None => println!("usage: /mode <suggest|auto-edit|full-auto|plan>"),
        },
        Some("resume") => {
            let arg = parts.next().unwrap_or("last");
            let r = if arg == "last" {
                host.session().resume_last().await
            } else {
                host.session().resume(arg).await
            };
            match r {
                Ok(m) => {
                    // History is re-derived from the event log each turn.
                    println!("(resumed {} with {} msgs)", host.session().current_id(), m.len());
                }
                Err(e) => println!("(resume: {e})"),
            }
        }
        Some("evolve") => {
            // `/evolve [--session <id>]` — distill the current (or given)
            // session into approved memory insights and skills. Direct
            // host call: no main-model turn is spent on dispatch.
            let mut sid: Option<String> = None;
            while let Some(arg) = parts.next() {
                if arg == "--session" {
                    sid = parts.next().map(str::to_string);
                }
            }
            match host.evolve(sid.as_deref()).await {
                Ok(out) => {
                    println!("( {} )", out.summarize());
                    for t in &out.added_insights {
                        println!("  + memory: {t}");
                    }
                    for t in &out.added_skills {
                        println!("  + skill:  {t}");
                    }
                    for t in &out.updated_skills {
                        println!("  ~ skill:  {t}");
                    }
                    for t in &out.retired {
                        println!("  - retired: {t}");
                    }
                    for t in out.rejected.iter().chain(&out.skipped) {
                        println!("  ! {t}");
                    }
                }
                Err(e) => println!("(evolve failed: {e})"),
            }
        }
        Some("sessions") => match host.session().list().await {
            Ok(list) => {
                if list.is_empty() {
                    println!("(no sessions)");
                }
                for s in list {
                    println!("{} ({} msgs)", s.id, s.msg_count);
                }
            }
            Err(e) => println!("(list: {e})"),
        },
        Some("reload-tools") => {
            // Same gathering as startup (ext tools rank after built-ins;
            // external + MCP are reloaded too - a reload that silently
            // skipped MCP would drift from the initial set).
            host.set_tools(gather_tools(all_tools.to_vec(), Vec::new(), false).await);
            println!("(reloaded tools)");
        }
        Some("help") => println!(
            "commands: /resume [id|last]  /clear  /mode <suggest|auto-edit|full-auto|plan>  /sessions  /evolve [--session <id>]  /reload-tools  /exit"
        ),
        _ => println!("unknown command; /help"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_banner_renders_evolve_payload_as_one_line() {
        let args = serde_json::json!({
            "action": "add insight",
            "title": "Prefer narrow grep over full reads",
            "tags": ["tooling", "rust"],
            "content": "Scan with targeted patterns before opening files."
        });
        let banner = approval_banner("evolve_write", &args);
        assert!(banner.contains("[approve evolve_write]"));
        assert!(banner.contains("add insight"));
        assert!(banner.contains("Prefer narrow grep over full reads"));
        // One JSON line + trailing newline; the y/N prompt follows below.
        assert!(banner.ends_with('\n'));
        assert_eq!(banner.lines().count(), 1);
    }

    #[test]
    fn approval_banner_empty_for_empty_args() {
        assert_eq!(approval_banner("bash", &serde_json::json!({})), "");
    }
}
