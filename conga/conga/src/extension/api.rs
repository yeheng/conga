//! `ExtensionApi` trait + `ExtensionApiImpl` - how extension crates register.
//!
//! Host/cli composition root calls `ext::register(&mut api)` for each linked
//! extension crate. See `docs/plugin-tutorial.md`.

use std::future::Future;
use std::pin::Pin;

use crate::types::message::ToolResultMessage;
use crate::types::tool::{RiskLevel, ToolCallVerdict, ToolDefinition};

/// A `before_tool_call` hook handler. Returns a verdict controlling flow.
pub trait BeforeToolCallHandler: Send + Sync {
    fn call(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        args: &serde_json::Value,
        risk: RiskLevel,
    ) -> ToolCallVerdict;
}

/// An `after_tool_call` hook handler. Returns a replacement result, or None to
/// leave it unchanged.
pub trait AfterToolCallHandler: Send + Sync {
    fn call(&self, tool_call_id: &str, result: &ToolResultMessage) -> Option<ToolResultMessage>;
}

/// The surface an extension crate uses to register capabilities with the agent.
///
/// **Events vs hooks are type-separated**: the agent loop emits events through
/// the `emit` closure passed to `run_agent_loop` (pure observation); hooks
/// registered here return a verdict / replacement that controls agent flow.
/// The two cannot be confused at the type level.
pub trait ExtensionApi: Send + Sync {
    /// Register a tool the LLM may call.
    fn register_tool(&mut self, tool: ToolDefinition);

    /// Register a `before_tool_call` hook (block / modify args).
    fn register_before_tool_call(&mut self, handler: Box<dyn BeforeToolCallHandler>);

    /// Register an `after_tool_call` hook (replace result).
    fn register_after_tool_call(&mut self, handler: Box<dyn AfterToolCallHandler>);
}

/// Concrete registry holding everything extension crates have registered.
#[derive(Default)]
pub struct ExtensionApiImpl {
    pub tools: Vec<ToolDefinition>,
    pub before_hooks: Vec<Box<dyn BeforeToolCallHandler>>,
    pub after_hooks: Vec<Box<dyn AfterToolCallHandler>>,
}

impl ExtensionApiImpl {
    pub fn new() -> Self {
        Self::default()
    }

    /// Run all `before_tool_call` hooks in registration order, combining their
    /// verdicts: the first `Block` wins; otherwise the last `Modify` wins;
    /// `Allow` is the default.
    pub fn before_tool_call(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        args: &serde_json::Value,
        risk: RiskLevel,
    ) -> ToolCallVerdict {
        let mut verdict = ToolCallVerdict::Allow;
        for h in &self.before_hooks {
            match h.call(tool_call_id, tool_name, args, risk) {
                v @ ToolCallVerdict::Block(_) => return v,
                v @ ToolCallVerdict::Modify(_) => verdict = v,
                ToolCallVerdict::Allow => {}
            }
        }
        verdict
    }

    /// Run all `after_tool_call` hooks in order, each may replace the result.
    pub fn after_tool_call(
        &self,
        tool_call_id: &str,
        result: &ToolResultMessage,
    ) -> ToolResultMessage {
        let mut current = result.clone();
        for h in &self.after_hooks {
            if let Some(replaced) = h.call(tool_call_id, &current) {
                current = replaced;
            }
        }
        current
    }
}

impl crate::types::tool::HookChain for ExtensionApiImpl {
    fn before_tool_call<'a>(
        &'a self,
        tool_call_id: &'a str,
        tool_name: &'a str,
        args: &'a serde_json::Value,
        risk: RiskLevel,
    ) -> Pin<Box<dyn Future<Output = ToolCallVerdict> + Send + 'a>> {
        Box::pin(async move {
            ExtensionApiImpl::before_tool_call(self, tool_call_id, tool_name, args, risk)
        })
    }

    fn after_tool_call(&self, tool_call_id: &str, result: &ToolResultMessage) -> ToolResultMessage {
        ExtensionApiImpl::after_tool_call(self, tool_call_id, result)
    }
}

impl ExtensionApi for ExtensionApiImpl {
    fn register_tool(&mut self, tool: ToolDefinition) {
        self.tools.push(tool);
    }

    fn register_before_tool_call(&mut self, handler: Box<dyn BeforeToolCallHandler>) {
        self.before_hooks.push(handler);
    }

    fn register_after_tool_call(&mut self, handler: Box<dyn AfterToolCallHandler>) {
        self.after_hooks.push(handler);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::message::{ContentBlock, ToolResultMessage};

    struct Blocker;
    impl BeforeToolCallHandler for Blocker {
        fn call(&self, _: &str, _: &str, _: &serde_json::Value, _: RiskLevel) -> ToolCallVerdict {
            ToolCallVerdict::Block("no".into())
        }
    }

    /// A minimal, valid tool definition.
    fn tool_def(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.to_string(),
            label: name.to_string(),
            description: format!("{name} tool"),
            parameters: serde_json::json!({"type": "object"}),
            risk: RiskLevel::Low,
            execute: std::sync::Arc::new(|_c: crate::types::tool::ToolCallCtx| {
                Box::pin(async { Ok(crate::types::tool::ToolResult::text("ok")) })
            }),
        }
    }

    #[test]
    fn before_hook_block_wins() {
        let mut api = ExtensionApiImpl::new();
        api.register_before_tool_call(Box::new(Blocker));
        let v = api.before_tool_call("id", "bash", &serde_json::json!({}), RiskLevel::High);
        assert!(matches!(v, ToolCallVerdict::Block(_)));
    }

    #[test]
    fn after_hook_replaces_result() {
        struct Redactor;
        impl AfterToolCallHandler for Redactor {
            fn call(&self, _: &str, _: &ToolResultMessage) -> Option<ToolResultMessage> {
                Some(ToolResultMessage {
                    tool_call_id: "id".into(),
                    tool_name: "x".into(),
                    content: vec![ContentBlock::text("[REDACTED]")],
                    is_error: false,
                    timestamp: 0,
                })
            }
        }
        let mut api = ExtensionApiImpl::new();
        api.register_after_tool_call(Box::new(Redactor));
        let orig = ToolResultMessage {
            tool_call_id: "id".into(),
            tool_name: "x".into(),
            content: vec![ContentBlock::text("secret")],
            is_error: false,
            timestamp: 0,
        };
        let out = api.after_tool_call("id", &orig);
        match &out.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "[REDACTED]"),
            _ => panic!(),
        }
    }

    /// Registered tools land in the registry in registration order.
    #[test]
    fn register_tool_appends_in_registration_order() {
        let mut api = ExtensionApiImpl::new();
        api.register_tool(tool_def("alpha"));
        api.register_tool(tool_def("beta"));
        assert_eq!(api.tools.len(), 2);
        assert_eq!(api.tools[0].name, "alpha");
        assert_eq!(api.tools[1].name, "beta");
        // The definition itself is stored verbatim (label, schema, risk).
        assert_eq!(api.tools[0].label, "alpha");
        assert_eq!(api.tools[0].risk, RiskLevel::Low);
        assert_eq!(
            api.tools[0].parameters,
            serde_json::json!({"type": "object"})
        );
    }

    /// The trait object is the surface extension crates use; registering
    /// through it must reach the same registry.
    #[test]
    fn register_tool_through_trait_object_reaches_the_registry() {
        let mut api = ExtensionApiImpl::new();
        {
            let dyn_api: &mut dyn ExtensionApi = &mut api;
            dyn_api.register_tool(tool_def("via-trait"));
        }
        assert_eq!(api.tools.len(), 1);
        assert_eq!(api.tools[0].name, "via-trait");
    }

    /// A fresh registry holds no tools / hooks.
    #[test]
    fn new_registry_is_empty() {
        let api = ExtensionApiImpl::new();
        assert!(api.tools.is_empty());
        assert!(api.before_hooks.is_empty());
        assert!(api.after_hooks.is_empty());
    }

    /// Duplicate names are NOT rejected or de-duplicated: both definitions
    /// stay in the registry (the first match wins at lookup time).
    #[test]
    fn register_tool_keeps_duplicate_names() {
        let mut api = ExtensionApiImpl::new();
        api.register_tool(tool_def("dup"));
        api.register_tool(tool_def("dup"));
        assert_eq!(api.tools.len(), 2, "the registry does not dedup by name");
        assert!(api.tools.iter().all(|t| t.name == "dup"));
    }

    /// The registry performs no validation: a definition without a name and
    /// without a parameters schema is accepted and stored as-is.
    #[test]
    fn register_tool_accepts_definition_without_name_or_schema() {
        let mut api = ExtensionApiImpl::new();
        let mut broken = tool_def("broken");
        broken.name = String::new();
        broken.parameters = serde_json::Value::Null;
        api.register_tool(broken);
        assert_eq!(api.tools.len(), 1);
        assert_eq!(api.tools[0].name, "");
        assert!(api.tools[0].parameters.is_null());
    }

    /// A handler that always returns the same verdict.
    struct FixedVerdict(ToolCallVerdict);
    impl BeforeToolCallHandler for FixedVerdict {
        fn call(&self, _: &str, _: &str, _: &serde_json::Value, _: RiskLevel) -> ToolCallVerdict {
            self.0.clone()
        }
    }

    /// Without a hook the call is allowed.
    #[test]
    fn before_hooks_allow_by_default() {
        let api = ExtensionApiImpl::new();
        let v = api.before_tool_call("id", "bash", &serde_json::json!({}), RiskLevel::Low);
        assert!(matches!(v, ToolCallVerdict::Allow));
    }

    /// The combination rules: the first Block wins, otherwise the LAST
    /// Modify wins, Allow leaves the verdict untouched.
    #[test]
    fn before_hooks_combine_verdicts() {
        // Allow + Modify -> Modify.
        let mut api = ExtensionApiImpl::new();
        api.register_before_tool_call(Box::new(FixedVerdict(ToolCallVerdict::Allow)));
        api.register_before_tool_call(Box::new(FixedVerdict(ToolCallVerdict::Modify(
            serde_json::json!({"n": 1}),
        ))));
        let v = api.before_tool_call("id", "bash", &serde_json::json!({}), RiskLevel::Low);
        match v {
            ToolCallVerdict::Modify(args) => assert_eq!(args, serde_json::json!({"n": 1})),
            other => panic!("expected Modify, got {other:?}"),
        }

        // Modify + Modify -> the last Modify wins.
        let mut api = ExtensionApiImpl::new();
        api.register_before_tool_call(Box::new(FixedVerdict(ToolCallVerdict::Modify(
            serde_json::json!({"n": 1}),
        ))));
        api.register_before_tool_call(Box::new(FixedVerdict(ToolCallVerdict::Modify(
            serde_json::json!({"n": 2}),
        ))));
        let v = api.before_tool_call("id", "bash", &serde_json::json!({}), RiskLevel::Low);
        match v {
            ToolCallVerdict::Modify(args) => assert_eq!(args, serde_json::json!({"n": 2})),
            other => panic!("expected Modify, got {other:?}"),
        }

        // A Block wins even when a Modify was seen before it.
        let mut api = ExtensionApiImpl::new();
        api.register_before_tool_call(Box::new(FixedVerdict(ToolCallVerdict::Modify(
            serde_json::json!({"n": 1}),
        ))));
        api.register_before_tool_call(Box::new(FixedVerdict(ToolCallVerdict::Block(
            "denied".into(),
        ))));
        let v = api.before_tool_call("id", "bash", &serde_json::json!({}), RiskLevel::Low);
        match v {
            ToolCallVerdict::Block(reason) => assert_eq!(reason, "denied"),
            other => panic!("expected Block, got {other:?}"),
        }
    }

    /// An after-hook that returns `None` keeps the current result; the next
    /// hook then sees the previous hook's replacement.
    #[test]
    fn after_hooks_chain_replacements_and_skip_none() {
        struct Skip;
        impl AfterToolCallHandler for Skip {
            fn call(&self, _: &str, _: &ToolResultMessage) -> Option<ToolResultMessage> {
                None
            }
        }
        struct Replace(&'static str);
        impl AfterToolCallHandler for Replace {
            fn call(&self, _: &str, _: &ToolResultMessage) -> Option<ToolResultMessage> {
                Some(ToolResultMessage {
                    tool_call_id: "id".into(),
                    tool_name: "x".into(),
                    content: vec![ContentBlock::text(self.0)],
                    is_error: false,
                    timestamp: 0,
                })
            }
        }
        let mut api = ExtensionApiImpl::new();
        api.register_after_tool_call(Box::new(Replace("first")));
        api.register_after_tool_call(Box::new(Skip));
        api.register_after_tool_call(Box::new(Replace("second")));

        let orig = ToolResultMessage {
            tool_call_id: "id".into(),
            tool_name: "x".into(),
            content: vec![ContentBlock::text("secret")],
            is_error: false,
            timestamp: 0,
        };
        let out = api.after_tool_call("id", &orig);
        match &out.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "second"),
            _ => panic!("expected text content"),
        }
    }
}
