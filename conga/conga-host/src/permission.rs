//! PermissionPolicy: 实装 core 的 HookChain，三档模式 + 工具风险 + approver 闭包。
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use conga::{CancelSignal, HookChain, RiskLevel, ToolCallVerdict, ToolResultMessage};
use parking_lot::RwLock as PlRwLock;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Suggest,
    AutoEdit,
    FullAuto,
    /// Read-only exploration + plan output: same tool gate as `Suggest`
    /// (Low-risk only) but framed as planning — mutating tools are blocked
    /// with a "present a plan" message and the approver is never consulted.
    /// The run_turn prompt injects a matching plan directive while active.
    Plan,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "suggest" => Some(Mode::Suggest),
            "auto-edit" | "autoedit" | "auto" => Some(Mode::AutoEdit),
            "full-auto" | "fullauto" | "full" => Some(Mode::FullAuto),
            "plan" => Some(Mode::Plan),
            _ => None,
        }
    }
}

/// 工具审批闭包：`(tool_name, args) -> 是否允许`。返回 future，宿主可
/// 挂起回合等待人工决策（CLI 读 stdin；gateway 走 WebSocket 往返）。
/// HRTB 允许 future 借用入参。
pub type Approver = Arc<
    dyn for<'a> Fn(
            &'a str,
            &'a serde_json::Value,
        ) -> Pin<Box<dyn Future<Output = bool> + Send + 'a>>
        + Send
        + Sync,
>;

pub struct PermissionPolicy {
    mode: AtomicU8,
    approver: Approver,
    /// Shared cancel signal (the Host's). When cancelled while waiting on
    /// the approver, the wait is abandoned immediately (event-driven -
    /// cancellation is centralized here instead of trusting every approver
    /// to be cancel-aware).
    signal: PlRwLock<Option<CancelSignal>>,
}

impl PermissionPolicy {
    pub fn new(mode: Mode, approver: Approver) -> Self {
        Self {
            mode: AtomicU8::new(mode as u8),
            approver,
            signal: PlRwLock::new(None),
        }
    }

    /// Attach the shared cancel signal (the Host's). Call once after the Host
    /// owning the signal is built; the policy is typically Arc-shared into
    /// the Host's hook chain before that point.
    pub fn set_signal(&self, signal: CancelSignal) {
        *self.signal.write() = Some(signal);
    }

    pub fn set_mode(&self, mode: Mode) {
        self.mode.store(mode as u8, Ordering::Relaxed);
    }

    pub fn mode(&self) -> Mode {
        match self.mode.load(Ordering::Relaxed) {
            0 => Mode::Suggest,
            1 => Mode::AutoEdit,
            2 => Mode::FullAuto,
            3 => Mode::Plan,
            _ => Mode::AutoEdit,
        }
    }

    /// Race the approver against cancellation. The HookChain contract
    /// requires human-blocking implementors to return promptly on cancel;
    /// an approver parked on stdin or a dead WS client cannot. The cancel
    /// branch is watch-backed, so the verdict returns the instant
    /// `cancel()` fires - no polling loop, no cancel latency.
    async fn await_approver(&self, name: &str, args: &serde_json::Value) -> ToolCallVerdict {
        let signal = self.signal.read().clone();
        let mut approver = std::pin::pin!((self.approver)(name, args));
        let allowed = match signal {
            Some(sig) => tokio::select! {
                biased;
                _ = sig.cancelled() => {
                    tracing::debug!("{name} approval wait cancelled");
                    return ToolCallVerdict::Block(format!("{name} aborted"));
                }
                allowed = &mut *approver => allowed,
            },
            None => approver.await,
        };
        if allowed {
            ToolCallVerdict::Allow
        } else {
            ToolCallVerdict::Block(format!("{name} denied by user"))
        }
    }

    /// Approve a non-tool-call action (evolve's per-candidate gate).
    /// Same cancel race as [`Self::await_approver`]: a parked approver
    /// unblocks instantly on cancel and counts as a rejection.
    pub async fn approve_action(&self, name: &str, args: &serde_json::Value) -> bool {
        let signal = self.signal.read().clone();
        let mut approver = std::pin::pin!((self.approver)(name, args));
        match signal {
            Some(sig) => tokio::select! {
                biased;
                _ = sig.cancelled() => false,
                allowed = &mut *approver => allowed,
            },
            None => approver.await,
        }
    }
}

impl HookChain for PermissionPolicy {
    fn before_tool_call<'a>(
        &'a self,
        _id: &'a str,
        name: &'a str,
        args: &'a serde_json::Value,
        risk: RiskLevel,
    ) -> Pin<Box<dyn Future<Output = ToolCallVerdict> + Send + 'a>> {
        Box::pin(async move {
            match (self.mode(), risk) {
                (Mode::FullAuto, _) => ToolCallVerdict::Allow,
                (Mode::AutoEdit, RiskLevel::Low) | (Mode::AutoEdit, RiskLevel::Medium) => {
                    ToolCallVerdict::Allow
                }
                (Mode::AutoEdit, RiskLevel::High) => self.await_approver(name, args).await,
                (Mode::Suggest, RiskLevel::Low) => ToolCallVerdict::Allow,
                (Mode::Suggest, RiskLevel::Medium) | (Mode::Suggest, RiskLevel::High) => {
                    ToolCallVerdict::Block("read-only mode".into())
                }
                (Mode::Plan, RiskLevel::Low) => ToolCallVerdict::Allow,
                (Mode::Plan, RiskLevel::Medium) | (Mode::Plan, RiskLevel::High) => {
                    ToolCallVerdict::Block(format!(
                        "{name} blocked: plan mode is read-only — present your plan as text"
                    ))
                }
            }
        })
    }

    fn after_tool_call(&self, _id: &str, result: &ToolResultMessage) -> ToolResultMessage {
        result.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn approver(allow: bool) -> (Approver, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let f: Approver = Arc::new(move |_n, _a| {
            c.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { allow })
        });
        (f, calls)
    }

    /// 调 before_tool_call 并取回 verdict（async 迁移后的统一入口）。
    async fn verdict(p: &PermissionPolicy, name: &str, risk: RiskLevel) -> ToolCallVerdict {
        p.before_tool_call("x", name, &serde_json::json!({}), risk)
            .await
    }

    #[tokio::test]
    async fn suggest_blocks_bash_and_writes() {
        let (a, _) = approver(false);
        let p = PermissionPolicy::new(Mode::Suggest, a);
        assert!(matches!(
            verdict(&p, "read", RiskLevel::Low).await,
            ToolCallVerdict::Allow
        ));
        assert!(matches!(
            verdict(&p, "write", RiskLevel::Medium).await,
            ToolCallVerdict::Block(_)
        ));
        assert!(matches!(
            verdict(&p, "bash", RiskLevel::High).await,
            ToolCallVerdict::Block(_)
        ));
    }

    #[tokio::test]
    async fn auto_edit_allows_writes_prompts_bash() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = calls.clone();
        let p = PermissionPolicy::new(
            Mode::AutoEdit,
            Arc::new(move |_, _| {
                c.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { true })
            }),
        );
        assert!(matches!(
            verdict(&p, "write", RiskLevel::Medium).await,
            ToolCallVerdict::Allow
        ));
        let v = verdict(&p, "bash", RiskLevel::High).await;
        assert!(matches!(v, ToolCallVerdict::Allow));
        assert_eq!(calls.load(Ordering::SeqCst), 1); // approver 被调用
    }

    #[tokio::test]
    async fn auto_edit_denies_bash_when_approver_rejects() {
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c = calls.clone();
        let p = PermissionPolicy::new(
            Mode::AutoEdit,
            Arc::new(move |_, _| {
                c.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { false })
            }),
        );
        assert!(matches!(
            verdict(&p, "write", RiskLevel::Medium).await,
            ToolCallVerdict::Allow
        ));
        let v = verdict(&p, "bash", RiskLevel::High).await;
        assert!(matches!(
            &v,
            ToolCallVerdict::Block(msg) if msg == "bash denied by user"
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 1); // approver 被调用
    }

    #[tokio::test]
    async fn full_auto_allows_everything() {
        let p = PermissionPolicy::new(Mode::FullAuto, Arc::new(|_, _| Box::pin(async { false })));
        assert!(matches!(
            verdict(&p, "bash", RiskLevel::High).await,
            ToolCallVerdict::Allow
        ));
    }

    #[tokio::test]
    async fn set_mode_switches_at_runtime() {
        let p = PermissionPolicy::new(Mode::Suggest, Arc::new(|_, _| Box::pin(async { false })));
        assert!(matches!(
            verdict(&p, "bash", RiskLevel::High).await,
            ToolCallVerdict::Block(_)
        ));
        p.set_mode(Mode::FullAuto);
        assert!(matches!(
            verdict(&p, "bash", RiskLevel::High).await,
            ToolCallVerdict::Allow
        ));
    }

    /// P1-5 regression: an approver that never resolves must not out-wait the
    /// cancel signal - the verdict must return promptly (event-driven, no
    /// poll interval) with a Block("…aborted").
    #[tokio::test]
    async fn approver_wait_is_cancel_aware() {
        let policy = PermissionPolicy::new(
            Mode::AutoEdit,
            Arc::new(|_n, _a| Box::pin(async { std::future::pending::<bool>().await })),
        );
        let signal = CancelSignal::new();
        policy.set_signal(signal.clone());

        // Cancel while the (never-resolving) approver is parked: the wait
        // must unwind within milliseconds, not on a poll deadline.
        let verdicts = tokio::spawn(async move {
            policy
                .before_tool_call("2", "bash", &serde_json::json!({}), RiskLevel::High)
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let started = std::time::Instant::now();
        signal.cancel();
        let verdict = verdicts.await.unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_millis(100),
            "cancel must unwind the approval wait immediately, took {:?}",
            started.elapsed()
        );
        match verdict {
            ToolCallVerdict::Block(msg) => assert!(msg.contains("aborted"), "{msg}"),
            v => panic!("expected Block, got {v:?}"),
        }
    }

    /// approve_action (evolve's per-candidate gate) shares the cancel race:
    /// a parked approver unblocks instantly on cancel and counts as false.
    #[tokio::test]
    async fn approve_action_cancel_counts_as_rejection() {
        let policy = PermissionPolicy::new(
            Mode::FullAuto,
            Arc::new(|_n, _a| Box::pin(async { std::future::pending::<bool>().await })),
        );
        let signal = CancelSignal::new();
        policy.set_signal(signal.clone());
        let waiting = tokio::spawn(async move {
            policy
                .approve_action(
                    "evolve_write",
                    &serde_json::json!({ "action": "add insight" }),
                )
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let started = std::time::Instant::now();
        signal.cancel();
        assert!(!waiting.await.unwrap(), "cancel must count as a rejection");
        assert!(
            started.elapsed() < std::time::Duration::from_millis(100),
            "cancel must unwind approve_action immediately, took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn parse_accepts_plan() {
        assert_eq!(Mode::parse("plan"), Some(Mode::Plan));
        assert_eq!(Mode::parse("PLAN"), Some(Mode::Plan));
    }

    /// Plan mode = Suggest's read-only gate with plan-flavored messaging:
    /// Low-risk tools run, everything mutating is blocked WITHOUT consulting
    /// the approver (there is nothing a human could approve in plan mode).
    #[tokio::test]
    async fn plan_mode_allows_readonly_blocks_mutating_without_approver() {
        let (a, calls) = approver(true);
        let p = PermissionPolicy::new(Mode::Plan, a);
        assert!(matches!(
            verdict(&p, "read", RiskLevel::Low).await,
            ToolCallVerdict::Allow
        ));
        for (name, risk) in [
            ("write", RiskLevel::Medium),
            ("edit", RiskLevel::Medium),
            ("fetch", RiskLevel::Medium),
            ("bash", RiskLevel::High),
        ] {
            match verdict(&p, name, risk).await {
                ToolCallVerdict::Block(msg) => {
                    assert!(msg.contains("plan mode"), "{name}: {msg}")
                }
                v => panic!("{name} must be blocked in plan mode, got {v:?}"),
            }
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "plan mode must not consult the approver"
        );
    }

    // ---- mode ----

    #[test]
    fn mode_reflects_the_configured_mode() {
        let p = PermissionPolicy::new(Mode::Suggest, Arc::new(|_, _| Box::pin(async { false })));
        assert_eq!(p.mode(), Mode::Suggest);
        p.set_mode(Mode::AutoEdit);
        assert_eq!(p.mode(), Mode::AutoEdit);
        p.set_mode(Mode::FullAuto);
        assert_eq!(p.mode(), Mode::FullAuto);
        p.set_mode(Mode::Plan);
        assert_eq!(p.mode(), Mode::Plan);
    }

    #[test]
    fn parse_accepts_the_documented_aliases() {
        assert_eq!(Mode::parse("suggest"), Some(Mode::Suggest));
        assert_eq!(Mode::parse("auto-edit"), Some(Mode::AutoEdit));
        assert_eq!(Mode::parse("autoedit"), Some(Mode::AutoEdit));
        assert_eq!(Mode::parse("auto"), Some(Mode::AutoEdit));
        assert_eq!(Mode::parse("full-auto"), Some(Mode::FullAuto));
        assert_eq!(Mode::parse("fullauto"), Some(Mode::FullAuto));
        assert_eq!(Mode::parse("full"), Some(Mode::FullAuto));
    }

    #[test]
    fn parse_rejects_unknown_modes() {
        assert_eq!(Mode::parse(""), None);
        assert_eq!(Mode::parse("nonsense"), None);
        assert_eq!(Mode::parse("full_auto"), None);
        assert_eq!(Mode::parse(" suggest"), None);
    }

    // ---- await_approver ----

    #[tokio::test]
    async fn await_approver_allows_when_the_approver_approves() {
        let p = PermissionPolicy::new(Mode::AutoEdit, Arc::new(|_, _| Box::pin(async { true })));
        assert!(matches!(
            p.await_approver("bash", &serde_json::json!({})).await,
            ToolCallVerdict::Allow
        ));
    }

    #[tokio::test]
    async fn await_approver_blocks_with_a_reason_when_the_approver_rejects() {
        let p = PermissionPolicy::new(Mode::AutoEdit, Arc::new(|_, _| Box::pin(async { false })));
        match p.await_approver("bash", &serde_json::json!({})).await {
            ToolCallVerdict::Block(msg) => {
                assert_eq!(msg, "bash denied by user")
            }
            v => panic!("expected Block, got {v:?}"),
        }
    }

    #[tokio::test]
    async fn await_approver_passes_the_tool_name_and_args_to_the_approver() {
        let seen: Arc<std::sync::Mutex<Vec<(String, String)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let c = seen.clone();
        let p = PermissionPolicy::new(
            Mode::AutoEdit,
            Arc::new(move |name, args| {
                c.lock().unwrap().push((name.to_string(), args.to_string()));
                Box::pin(async { true })
            }),
        );
        p.await_approver("bash", &serde_json::json!({"cmd": "ls"}))
            .await;
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, "bash");
        assert!(
            seen[0].1.contains("ls"),
            "the args must reach the approver, got: {}",
            seen[0].1
        );
    }

    #[tokio::test]
    async fn await_approver_reports_cancellation_as_an_abort() {
        let policy = PermissionPolicy::new(
            Mode::AutoEdit,
            Arc::new(|_n, _a| Box::pin(std::future::pending::<bool>())),
        );
        let signal = CancelSignal::new();
        policy.set_signal(signal.clone());
        let waiting =
            tokio::spawn(
                async move { policy.await_approver("bash", &serde_json::json!({})).await },
            );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        let started = std::time::Instant::now();
        signal.cancel();
        match waiting.await.unwrap() {
            ToolCallVerdict::Block(msg) => {
                assert!(msg.contains("aborted"), "{msg}");
                assert!(msg.contains("bash"), "{msg}");
            }
            v => panic!("expected Block, got {v:?}"),
        }
        assert!(
            started.elapsed() < std::time::Duration::from_millis(100),
            "cancel must unwind the approval wait immediately"
        );
    }

    // ---- after_tool_call ----

    #[test]
    fn after_tool_call_passes_the_result_through_unchanged() {
        let p = PermissionPolicy::new(Mode::FullAuto, Arc::new(|_, _| Box::pin(async { true })));
        let result = ToolResultMessage {
            tool_call_id: "t1".into(),
            tool_name: "bash".into(),
            content: vec![conga::ContentBlock::text("ok")],
            is_error: false,
            timestamp: 7,
        };
        let out = p.after_tool_call("call-1", &result);
        assert_eq!(out, result);
        assert_eq!(out.tool_call_id, "t1");
        assert_eq!(out.timestamp, 7);
    }

    #[test]
    fn after_tool_call_returns_an_independent_clone() {
        let p = PermissionPolicy::new(Mode::FullAuto, Arc::new(|_, _| Box::pin(async { true })));
        let mut result = ToolResultMessage {
            tool_call_id: "t1".into(),
            tool_name: "bash".into(),
            content: vec![conga::ContentBlock::text("ok")],
            is_error: false,
            timestamp: 0,
        };
        let out = p.after_tool_call("call-1", &result);
        result
            .content
            .push(conga::ContentBlock::text("late mutation"));
        assert_eq!(
            out.content.len(),
            1,
            "the returned message must be a clone, not a shared handle"
        );
    }
}
