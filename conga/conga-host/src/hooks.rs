//! Compose multiple [`HookChain`]s (e.g. PermissionPolicy + ExtensionApiImpl).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use conga::{HookChain, RiskLevel, ToolCallVerdict, ToolResultMessage};

/// Runs hook chains in order.
///
/// `before`: first `Block` wins; `Modify` updates args for later chains;
/// final verdict is last `Modify` or `Allow`.
/// `after`: each chain may replace the result in sequence.
pub struct HookStack {
    chains: Vec<Arc<dyn HookChain>>,
}

impl HookStack {
    pub fn new(chains: Vec<Arc<dyn HookChain>>) -> Self {
        Self { chains }
    }

    pub fn push(&mut self, chain: Arc<dyn HookChain>) {
        self.chains.push(chain);
    }
}

impl HookChain for HookStack {
    fn before_tool_call<'a>(
        &'a self,
        tool_call_id: &'a str,
        tool_name: &'a str,
        args: &'a serde_json::Value,
        risk: RiskLevel,
    ) -> Pin<Box<dyn Future<Output = ToolCallVerdict> + Send + 'a>> {
        Box::pin(async move {
            let mut current = args.clone();
            let mut modified = false;
            for chain in &self.chains {
                match chain
                    .before_tool_call(tool_call_id, tool_name, &current, risk)
                    .await
                {
                    ToolCallVerdict::Block(reason) => return ToolCallVerdict::Block(reason),
                    ToolCallVerdict::Modify(a) => {
                        current = a;
                        modified = true;
                    }
                    ToolCallVerdict::Allow => {}
                }
            }
            if modified {
                ToolCallVerdict::Modify(current)
            } else {
                ToolCallVerdict::Allow
            }
        })
    }

    fn after_tool_call(&self, tool_call_id: &str, result: &ToolResultMessage) -> ToolResultMessage {
        let mut current = result.clone();
        for chain in &self.chains {
            current = chain.after_tool_call(tool_call_id, &current);
        }
        current
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use conga::{ContentBlock, ToolResultMessage};

    struct BlockBash;
    impl HookChain for BlockBash {
        fn before_tool_call<'a>(
            &'a self,
            _: &'a str,
            name: &'a str,
            _: &'a serde_json::Value,
            _: RiskLevel,
        ) -> Pin<Box<dyn Future<Output = ToolCallVerdict> + Send + 'a>> {
            Box::pin(async move {
                if name == "bash" {
                    ToolCallVerdict::Block("no bash".into())
                } else {
                    ToolCallVerdict::Allow
                }
            })
        }
        fn after_tool_call(&self, _: &str, r: &ToolResultMessage) -> ToolResultMessage {
            r.clone()
        }
    }

    struct AllowAll;
    impl HookChain for AllowAll {
        fn before_tool_call<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: &'a serde_json::Value,
            _: RiskLevel,
        ) -> Pin<Box<dyn Future<Output = ToolCallVerdict> + Send + 'a>> {
            Box::pin(async { ToolCallVerdict::Allow })
        }
        fn after_tool_call(&self, _: &str, r: &ToolResultMessage) -> ToolResultMessage {
            r.clone()
        }
    }

    struct Redact;
    impl HookChain for Redact {
        fn before_tool_call<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: &'a serde_json::Value,
            _: RiskLevel,
        ) -> Pin<Box<dyn Future<Output = ToolCallVerdict> + Send + 'a>> {
            Box::pin(async { ToolCallVerdict::Allow })
        }
        fn after_tool_call(&self, _: &str, r: &ToolResultMessage) -> ToolResultMessage {
            ToolResultMessage {
                tool_call_id: r.tool_call_id.clone(),
                tool_name: r.tool_name.clone(),
                content: vec![conga::ContentBlock::text("[x]".to_string())],
                is_error: r.is_error,
                timestamp: r.timestamp,
            }
        }
    }

    struct ModifyArgs;
    impl HookChain for ModifyArgs {
        fn before_tool_call<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: &'a serde_json::Value,
            _: RiskLevel,
        ) -> Pin<Box<dyn Future<Output = ToolCallVerdict> + Send + 'a>> {
            Box::pin(async { ToolCallVerdict::Modify(serde_json::json!({"rewritten": true})) })
        }
        fn after_tool_call(&self, _: &str, r: &ToolResultMessage) -> ToolResultMessage {
            r.clone()
        }
    }

    #[tokio::test]
    async fn push_appends_the_chain_to_the_stack() {
        let mut stack = HookStack::new(Vec::new());
        // An empty stack allows everything.
        let v = stack
            .before_tool_call("1", "bash", &serde_json::json!({}), RiskLevel::High)
            .await;
        assert!(matches!(v, ToolCallVerdict::Allow));

        // A pushed chain is consulted from then on.
        stack.push(Arc::new(BlockBash));
        let v = stack
            .before_tool_call("1", "bash", &serde_json::json!({}), RiskLevel::High)
            .await;
        assert!(matches!(v, ToolCallVerdict::Block(_)), "the pushed chain must run");

        // Pushing more chains keeps the earlier verdict (first Block wins).
        stack.push(Arc::new(AllowAll));
        let v = stack
            .before_tool_call("1", "bash", &serde_json::json!({}), RiskLevel::High)
            .await;
        assert!(matches!(v, ToolCallVerdict::Block(_)));
    }

    #[tokio::test]
    async fn pushed_modify_chain_rewrites_the_arguments() {
        let mut stack = HookStack::new(vec![Arc::new(AllowAll)]);
        stack.push(Arc::new(ModifyArgs));
        let v = stack
            .before_tool_call("1", "bash", &serde_json::json!({"cmd": "ls"}), RiskLevel::Low)
            .await;
        match v {
            ToolCallVerdict::Modify(args) => assert_eq!(args["rewritten"], true),
            other => panic!("expected Modify, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn first_block_wins() {
        let stack = HookStack::new(vec![Arc::new(AllowAll), Arc::new(BlockBash)]);
        let v = stack
            .before_tool_call("1", "bash", &serde_json::json!({}), RiskLevel::High)
            .await;
        assert!(matches!(v, ToolCallVerdict::Block(_)));
    }

    #[test]
    fn after_pipes() {
        let stack = HookStack::new(vec![Arc::new(Redact)]);
        let orig = ToolResultMessage {
            tool_call_id: "1".into(),
            tool_name: "t".into(),
            content: vec![ContentBlock::text("secret")],
            is_error: false,
            timestamp: 0,
        };
        let out = stack.after_tool_call("1", &orig);
        match &out.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "[x]"),
            _ => panic!(),
        }
    }
}
