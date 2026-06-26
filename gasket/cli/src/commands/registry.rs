//! Common registry building utilities shared between gateway and agent commands.
//!
//! This module eliminates duplicate registration logic for skills, and markdown loading.

use gasket_engine::config::{Config, ModelRegistry};
use gasket_engine::providers::ProviderRegistry;
use gasket_engine::AgentConfig;

/// CLI-level implementation of ModelResolver using ProviderRegistry + ModelRegistry.
///
/// This resolves model_id strings (e.g., "minimax", "minimax/abab6.5-chat",
/// or named profiles like "smart-assistant") to actual provider + config pairs
/// for subagent model switching.
pub struct CliModelResolver {
    pub provider_registry: ProviderRegistry,
    pub model_registry: ModelRegistry,
}

impl gasket_engine::ModelResolver for CliModelResolver {
    fn resolve_model(
        &self,
        model_id: &str,
    ) -> Option<(
        std::sync::Arc<dyn gasket_engine::providers::LlmProvider>,
        gasket_engine::AgentConfig,
    )> {
        // 1. Try to resolve from named model profiles (e.g., "smart-assistant")
        if let Some((_id, profile)) = self
            .model_registry
            .get_profile_with_fallback(Some(model_id))
        {
            let provider_name = profile.provider.clone();
            let provider = self.provider_registry.get_or_create(&provider_name).ok()?;

            let config = gasket_engine::AgentConfig {
                model: profile.model.clone(),
                temperature: profile.temperature.unwrap_or(1.0),
                max_tokens: profile.max_tokens.unwrap_or(65536),
                ..Default::default()
            };

            return Some((provider, config));
        }

        // 2. Try "provider/model" format (e.g., "minimax/abab6.5-chat")
        let parts: Vec<&str> = model_id.splitn(2, '/').collect();
        if parts.len() == 2 {
            let provider_name = parts[0];
            let model_name = parts[1];

            if let Ok(provider) = self.provider_registry.get_or_create(provider_name) {
                let config = gasket_engine::AgentConfig {
                    model: model_name.to_string(),
                    ..Default::default()
                };
                return Some((provider, config));
            }
        }

        // 3. Try as bare provider name (e.g., "minimax" → use provider's default model)
        if let Ok(provider) = self.provider_registry.get_or_create(model_id) {
            let config = gasket_engine::AgentConfig {
                model: provider.default_model().to_string(),
                ..Default::default()
            };
            return Some((provider, config));
        }

        None
    }
}

/// Build AgentConfig from the config file, applying defaults for zero-valued fields.
pub fn build_agent_config(config: &Config) -> AgentConfig {
    let defaults = AgentConfig::default();
    AgentConfig {
        model: String::new(), // caller overrides with resolved model
        max_iterations: match config.agents.defaults.max_iterations {
            0 => defaults.max_iterations,
            v => v,
        },
        temperature: config.agents.defaults.temperature,
        max_tokens: match config.agents.defaults.max_tokens {
            0 => defaults.max_tokens,
            v => v,
        },
        memory_window: match config.agents.defaults.memory_window {
            0 => defaults.memory_window,
            v => v,
        },
        max_tool_result_chars: defaults.max_tool_result_chars,
        max_retries: defaults.max_retries,
        thinking_enabled: config.agents.defaults.thinking_enabled,
        streaming: config.agents.defaults.streaming,
        tool_timeout_secs: defaults.tool_timeout_secs,
        plugin_timeout_secs: match config.agents.defaults.plugin_timeout_secs {
            0 => defaults.plugin_timeout_secs,
            v => v,
        },
        subagent_timeout_secs: defaults.subagent_timeout_secs,
        session_idle_timeout_secs: defaults.session_idle_timeout_secs,
        compaction_cooldown_secs: defaults.compaction_cooldown_secs,
        after_response_hook_timeout_secs: defaults.after_response_hook_timeout_secs,
        external_hook_timeout_secs: defaults.external_hook_timeout_secs,
        ws_summary_limit: config.agents.defaults.ws_summary_limit,
        prompts: {
            let p = &config.agents.defaults.prompts;
            gasket_engine::session::config::PromptsConfig {
                identity_prefix: p.identity_prefix.clone(),
                summarization: p.summarization.clone(),
                checkpoint: p.checkpoint.clone(),
                evolution: p.evolution.clone(),
                planning: p.planning.clone(),
                distill: p.distill.clone(),
            }
        },
        evolution: defaults.evolution,
        stop_words_path: config.stop_words_path.clone(),
    }
}
