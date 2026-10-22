//! 审批请求登记：id → oneshot 决策通道 + 按工具名的 remember 缓存。
//! 纯逻辑、无 IO，可单测；传输胶水在各宿主的 approver 闭包里
//! （gateway 走 WebSocket 往返，Tauri 桌面端走 IPC 事件）。

use std::collections::HashMap;

use tokio::sync::oneshot;

/// 一次 `register` 的结果。
#[derive(Debug)]
pub enum RegisterOutcome {
    /// 该工具此前被 remember，直接复用历史决策。
    Remembered(bool),
    /// 需要人工审批：request_id 用于回填决策，rx 是等待端。
    Pending {
        request_id: String,
        rx: oneshot::Receiver<bool>,
    },
}

/// 追踪在途审批与 remember 决策。同一时刻至多一个在途审批
/// （execute_tool_calls 串行 await hook），但设计上不依赖这个假设。
pub struct ApprovalRegistry {
    pending: HashMap<String, (String, oneshot::Sender<bool>)>,
    memory: HashMap<String, bool>,
    seq: u64,
}

impl Default for ApprovalRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ApprovalRegistry {
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
            memory: HashMap::new(),
            seq: 0,
        }
    }

    /// 注册一次审批。remember 命中时直接返回历史决策；否则分配
    /// 自增 request_id（格式 `ap{seq}`）并登记决策通道。
    pub fn register(&mut self, tool_name: &str) -> RegisterOutcome {
        if let Some(decided) = self.memory.get(tool_name) {
            return RegisterOutcome::Remembered(*decided);
        }
        self.seq += 1;
        let request_id = format!("ap{}", self.seq);
        let (tx, rx) = oneshot::channel();
        self.pending
            .insert(request_id.clone(), (tool_name.to_string(), tx));
        RegisterOutcome::Pending { request_id, rx }
    }

    /// 回填决策。未知 request_id 静默忽略（迟到/重复响应）。
    /// `remember=true` 时按工具名缓存决策供后续复用。
    pub fn respond(&mut self, request_id: &str, approved: bool, remember: bool) {
        if let Some((tool_name, tx)) = self.pending.remove(request_id) {
            let _ = tx.send(approved);
            if remember {
                self.memory.insert(tool_name, approved);
            }
        }
    }

    /// 回合结束时清空在途审批：sender 全部 drop，等待端 select!
    /// 的 oneshot 分支以 Err 立即返回（调用方按 false 处理），不会挂起。
    pub fn clear_pending(&mut self) {
        self.pending.clear();
    }

    /// Drop the remember cache. Called on permission-mode changes: a
    /// downgrade (e.g. full-auto -> auto-edit) must not keep honoring a
    /// decision remembered under the looser mode — those tools need human
    /// approval again.
    pub fn clear_memory(&mut self) {
        self.memory.clear();
    }
}

/// 等待审批决策：oneshot 响应 / cancel 通知 / 超时 三路，任一先到即返回。
/// `cancel_rx` 必须是 subscribe 出的新鲜 receiver（见 wait_for_decision 测试：
/// 取消后新审批不得被旧信号毒化）。
pub async fn wait_for_decision(
    rx: oneshot::Receiver<bool>,
    mut cancel_rx: tokio::sync::watch::Receiver<bool>,
    timeout: std::time::Duration,
) -> bool {
    tokio::select! {
        r = rx => r.unwrap_or(false),
        _ = cancel_rx.changed() => false,
        _ = tokio::time::sleep(timeout) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_then_respond_resolves() {
        let mut r = ApprovalRegistry::new();
        let RegisterOutcome::Pending { request_id, rx } = r.register("bash") else {
            panic!("first approval must be pending");
        };
        assert_eq!(request_id, "ap1");
        r.respond(&request_id, true, false);
        assert_eq!(rx.blocking_recv(), Ok(true));
    }

    #[test]
    fn remembered_decision_bypasses_approval() {
        let mut r = ApprovalRegistry::new();
        let RegisterOutcome::Pending { request_id, .. } = r.register("bash") else {
            panic!();
        };
        r.respond(&request_id, true, true); // remember=true
        match r.register("bash") {
            RegisterOutcome::Remembered(true) => {}
            other => panic!("expected remembered(true), got {other:?}"),
        }
        // 其他工具不受影响
        assert!(matches!(
            r.register("write"),
            RegisterOutcome::Pending { .. }
        ));
    }

    #[test]
    fn duplicate_response_is_ignored() {
        let mut r = ApprovalRegistry::new();
        let RegisterOutcome::Pending { request_id, rx } = r.register("bash") else {
            panic!();
        };
        r.respond(&request_id, true, false);
        r.respond(&request_id, false, false); // 第二次：no-op
        assert_eq!(rx.blocking_recv(), Ok(true), "first decision wins");
    }

    #[test]
    fn unknown_request_id_is_ignored() {
        let mut r = ApprovalRegistry::new();
        r.respond("ap999", true, false); // 不 panic
    }

    #[test]
    fn clear_pending_drops_waiters() {
        let mut r = ApprovalRegistry::new();
        let RegisterOutcome::Pending { rx, .. } = r.register("bash") else {
            panic!();
        };
        r.clear_pending();
        // tokio 1.53 的 RecvError 是无公开构造函数的单元结构体，
        // 只能断言收端以 Err（通道关闭）返回。
        assert!(rx.blocking_recv().is_err());
    }

    #[test]
    fn clear_memory_drops_remembered_decisions() {
        let mut r = ApprovalRegistry::new();
        let RegisterOutcome::Pending { request_id, .. } = r.register("bash") else {
            panic!();
        };
        r.respond(&request_id, true, true); // remember allow
        assert!(matches!(
            r.register("bash"),
            RegisterOutcome::Remembered(true)
        ));
        // Mode change: remembered approvals must not outlive the switch —
        // the same tool asks for approval again.
        r.clear_memory();
        assert!(matches!(
            r.register("bash"),
            RegisterOutcome::Pending { .. }
        ));
        // Pending tracking is untouched by clear_memory.
        let RegisterOutcome::Pending { rx, .. } = r.register("write") else {
            panic!();
        };
        r.clear_pending();
        assert!(rx.blocking_recv().is_err());
    }

    #[test]
    fn seq_increments_per_request() {
        let mut r = ApprovalRegistry::new();
        let RegisterOutcome::Pending { request_id, .. } = r.register("bash") else {
            panic!();
        };
        assert_eq!(request_id, "ap1");
        let RegisterOutcome::Pending { request_id, .. } = r.register("write") else {
            panic!();
        };
        assert_eq!(request_id, "ap2");
    }

    /// 闩锁回归测试：订阅前的 cancel 不得命中 subscribe() 出的新 receiver——
    /// 只有订阅之后的 send 才能解锁（返回 false）。旧实现用 Receiver::clone()
    /// 每次复制旧 observed-version，第一次 cancel 后所有后续审批立即被拒。
    #[tokio::test]
    async fn cancel_before_subscribe_is_not_latched() {
        let (cancel_tx, _old_rx) = tokio::sync::watch::channel(false);
        // 先发送一次 cancel（模拟本连接早前的取消）
        cancel_tx.send(true).unwrap();
        let cancel_rx = cancel_tx.subscribe();
        let (_decision_tx, decision_rx) = oneshot::channel();
        let task = tokio::spawn(wait_for_decision(
            decision_rx,
            cancel_rx,
            std::time::Duration::from_secs(60),
        ));
        // 让等待任务至少轮询一次：若 subscribe 被旧版本毒化，此刻应已 resolve(false)。
        tokio::task::yield_now().await;
        assert!(
            !task.is_finished(),
            "订阅前的 cancel 不得毒化新订阅（闩锁回归）"
        );
        // 只有后续 send 才能解锁 → false
        cancel_tx.send(true).unwrap();
        assert!(!task.await.unwrap());
    }

    /// 等待开始之后才收到 cancel → 立即按拒绝处理。
    #[tokio::test]
    async fn cancel_after_wait_starts_resolves_false() {
        let (cancel_tx, _old_rx) = tokio::sync::watch::channel(false);
        let cancel_rx = cancel_tx.subscribe();
        let (_decision_tx, decision_rx) = oneshot::channel();
        let task = tokio::spawn(wait_for_decision(
            decision_rx,
            cancel_rx,
            std::time::Duration::from_secs(60),
        ));
        tokio::task::yield_now().await;
        cancel_tx.send(true).unwrap();
        assert!(!task.await.unwrap());
    }

    /// oneshot 决策通道的响应原样透出（true / false 两条路径）。
    #[tokio::test]
    async fn oneshot_response_resolves_value() {
        for expected in [true, false] {
            let (decision_tx, decision_rx) = oneshot::channel();
            let (cancel_tx, _old_rx) = tokio::sync::watch::channel(false);
            let cancel_rx = cancel_tx.subscribe();
            let task = tokio::spawn(wait_for_decision(
                decision_rx,
                cancel_rx,
                std::time::Duration::from_secs(60),
            ));
            decision_tx.send(expected).unwrap();
            assert_eq!(task.await.unwrap(), expected);
        }
    }

    /// 三路都未触发时，超时窗口结束后按拒绝处理，且不提前返回。
    #[tokio::test]
    async fn timeout_resolves_false() {
        let (_decision_tx, decision_rx) = oneshot::channel();
        let (cancel_tx, _old_rx) = tokio::sync::watch::channel(false);
        let cancel_rx = cancel_tx.subscribe();
        let start = std::time::Instant::now();
        let result =
            wait_for_decision(decision_rx, cancel_rx, std::time::Duration::from_millis(10)).await;
        assert!(!result, "超时按拒绝处理");
        assert!(
            start.elapsed() >= std::time::Duration::from_millis(10),
            "10ms 超时不应提前返回"
        );
    }

    // ---- Default ----

    #[test]
    fn default_registry_behaves_like_a_new_one() {
        let mut r = ApprovalRegistry::default();
        let RegisterOutcome::Pending { request_id, rx } = r.register("bash") else {
            panic!("first approval must be pending");
        };
        assert_eq!(request_id, "ap1", "the id sequence starts at ap1");
        r.respond(&request_id, true, true);
        assert_eq!(rx.blocking_recv(), Ok(true));
        assert!(
            matches!(r.register("bash"), RegisterOutcome::Remembered(true)),
            "a default registry remembers like a new one"
        );
    }

    #[test]
    fn default_registry_has_an_empty_remember_cache() {
        let mut r = ApprovalRegistry::default();
        // Nothing remembered yet → the first approval is pending, not reused.
        assert!(matches!(
            r.register("bash"),
            RegisterOutcome::Pending { .. }
        ));
    }

    // ---- remember semantics ----

    #[test]
    fn a_decision_without_remember_is_not_reused() {
        let mut r = ApprovalRegistry::new();
        let RegisterOutcome::Pending { request_id, rx } = r.register("bash") else {
            panic!();
        };
        r.respond(&request_id, true, false); // remember=false
        assert_eq!(rx.blocking_recv(), Ok(true));
        assert!(
            matches!(r.register("bash"), RegisterOutcome::Pending { .. }),
            "without remember the same tool must ask again"
        );
    }

    #[test]
    fn a_remembered_rejection_is_reused() {
        let mut r = ApprovalRegistry::new();
        let RegisterOutcome::Pending { request_id, rx } = r.register("bash") else {
            panic!();
        };
        r.respond(&request_id, false, true);
        assert_eq!(rx.blocking_recv(), Ok(false));
        match r.register("bash") {
            RegisterOutcome::Remembered(false) => {}
            other => panic!("expected remembered(false), got {other:?}"),
        }
    }

    #[test]
    fn a_response_after_clear_pending_is_ignored() {
        // A late response must neither panic nor resurrect the decision.
        let mut r = ApprovalRegistry::new();
        let RegisterOutcome::Pending { request_id, rx } = r.register("bash") else {
            panic!();
        };
        r.clear_pending();
        r.respond(&request_id, true, true);
        assert!(rx.blocking_recv().is_err(), "the waiter stays closed");
        assert!(
            matches!(r.register("bash"), RegisterOutcome::Pending { .. }),
            "a late response must not remember anything"
        );
    }
}
