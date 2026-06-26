//! Application-level configuration types (YAML config file layer)
//!
//! These types define the **file format** of `~/.gasket/config.yaml`. They are
//! the primary config layer used by the CLI to load, validate, and save
//! configuration.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::config::config_dir;
use crate::error::ConfigValidationError;
use crate::vault::contains_placeholders;
use crate::vault::VaultStore;

// Re-export channel config types (from gasket-types to avoid engine -> channels dep)
pub use gasket_types::channel_config::ChannelsConfig;
// Re-export tools config
use crate::config::ToolsConfig;

// Re-export provider config types (unified in gasket-providers)
pub use gasket_providers::{ModelConfig, ModelSpec, ProviderConfig, ProviderType};

/// Agent profile for model switching
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelProfile {
    pub model: String,
    pub provider: String,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default, alias = "maxTokens")]
    pub max_tokens: Option<u32>,
    #[serde(default, alias = "thinkingEnabled")]
    pub thinking_enabled: Option<bool>,
}

/// Prompt templates and overrides for internal AI behaviors (YAML layer).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PromptsConfig {
    /// Identity prefix injected before bootstrap files in the system prompt.
    #[serde(default)]
    pub identity_prefix: Option<String>,
    /// System prompt used by ContextCompactor for summarization.
    #[serde(default)]
    pub summarization: Option<String>,
    /// User prompt template used by ContextCompactor for checkpoint generation.
    #[serde(default)]
    pub checkpoint: Option<String>,
    /// User prompt template used by EvolutionTool for memory extraction.
    /// Must contain `{{conversation}}` which will be replaced with the transcript.
    #[serde(default)]
    pub evolution: Option<String>,
    /// User prompt template used by EvolutionTool for distill meta-analysis.
    /// Must contain `{{memories}}` which will be replaced with the extracted memories JSON.
    #[serde(default)]
    pub distill: Option<String>,
    /// User prompt template used by CreatePlanTool for plan generation.
    /// Must contain `{{goal}}` and `{{context}}` which will be replaced at runtime.
    #[serde(default)]
    pub planning: Option<String>,
}

/// Agent defaults configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDefaults {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub temperature: f32,
    #[serde(default, alias = "maxTokens")]
    pub max_tokens: u32,
    #[serde(default, alias = "maxIterations")]
    pub max_iterations: u32,
    #[serde(default, alias = "memoryWindow")]
    pub memory_window: usize,
    #[serde(default, alias = "thinkingEnabled")]
    pub thinking_enabled: bool,
    #[serde(default)]
    pub streaming: bool,
    #[serde(default, alias = "wsSummaryLimit")]
    pub ws_summary_limit: usize,
    #[serde(default, alias = "pluginTimeoutSecs")]
    pub plugin_timeout_secs: u64,
    #[serde(default)]
    pub prompts: PromptsConfig,
}

impl Default for AgentDefaults {
    fn default() -> Self {
        Self {
            model: None,
            temperature: 0.0,
            max_tokens: 0,
            max_iterations: 0,
            memory_window: 0,
            thinking_enabled: false,
            streaming: true,
            ws_summary_limit: 0,
            plugin_timeout_secs: 0,
            prompts: PromptsConfig::default(),
        }
    }
}

/// Agents configuration section
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentsConfig {
    #[serde(default)]
    pub defaults: AgentDefaults,
    #[serde(default)]
    pub models: HashMap<String, ModelProfile>,
}

#[cfg(feature = "embedding")]
fn default_hot_limit() -> usize {
    1000
}

/// Embedding configuration (only available with `embedding` feature).
#[cfg(feature = "embedding")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingConfig {
    pub provider: gasket_embedding::ProviderConfig,
    #[serde(default)]
    pub recall: gasket_embedding::RecallConfig,
    /// Maximum number of recent embeddings to keep in the in-memory hot index.
    /// 0 = disable memory index entirely (pure cold store).
    #[serde(default = "default_hot_limit")]
    pub hot_limit: usize,
    /// Vector store backend selection. Defaults to LanceDB at "~/.gasket/vectors".
    #[serde(default)]
    pub vector_store: gasket_embedding::VectorStoreConfig,
}

/// Root configuration structure — maps directly to `~/.gasket/config.yaml`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
    #[serde(default)]
    pub agents: AgentsConfig,
    #[serde(default)]
    pub channels: ChannelsConfig,
    #[serde(default)]
    pub tools: ToolsConfig,
    #[cfg(feature = "embedding")]
    #[serde(default)]
    pub embedding: Option<EmbeddingConfig>,
    /// Optional path to an external stop-words file for keyword-based history recall.
    /// When absent or unreadable, the built-in bilingual dictionary is used.
    #[serde(default, alias = "stopWordsPath")]
    pub stop_words_path: Option<std::path::PathBuf>,
}

impl Config {
    pub fn validate(&self) -> Result<(), Vec<ConfigValidationError>> {
        let mut errors = Vec::new();
        for (name, provider) in &self.providers {
            if !provider.is_available(name) {
                errors.push(ConfigValidationError::ProviderNotAvailable(name.clone()));
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}

/// Load configuration from file.
///
/// Vault placeholders (`{{vault:key}}`) are kept as raw strings in the
/// resulting `Config`. They are resolved at the point of use (JIT) via
/// `VaultStore::resolve_text()`.
///
/// **Important:** `{{vault:key}}` values in YAML must be quoted, e.g.
/// `api_key: "{{vault:openai_key}}"`, otherwise YAML parsing will fail.
pub async fn load_config() -> anyhow::Result<Config> {
    let config_path = config_path()?;
    if !config_path.exists() {
        return Ok(Config::default());
    }
    let content = tokio::fs::read_to_string(&config_path).await?;
    let config: Config = serde_yaml::from_str(&content).map_err(|e| {
        // Provide a helpful hint when YAML parsing fails and the raw text
        // contains unquoted vault placeholders.
        if contains_placeholders(&content) {
            anyhow::anyhow!(
                "{e}\n\nHint: vault placeholders must be quoted in YAML.\n\
                 Use  api_key: \"{{{{vault:key}}}}\"  instead of  api_key: {{{{vault:key}}}}"
            )
        } else {
            anyhow::anyhow!(e)
        }
    })?;
    Ok(config)
}

/// Get the config file path
pub fn config_path() -> std::io::Result<std::path::PathBuf> {
    Ok(config_dir().join("config.yaml"))
}

/// Configuration loader — reads/writes `~/.gasket/config.yaml`.
pub struct ConfigLoader {
    config_path: std::path::PathBuf,
}

impl ConfigLoader {
    pub fn new() -> Self {
        Self {
            config_path: config_path().unwrap_or_else(|_| config_dir().join("config.yaml")),
        }
    }

    pub async fn load(&self) -> anyhow::Result<Config> {
        load_config().await
    }

    pub async fn save(&self, config: &Config) -> anyhow::Result<()> {
        let content = serde_yaml::to_string(config)?;
        tokio::fs::write(&self.config_path, content).await?;
        Ok(())
    }

    pub fn config_path(&self) -> &std::path::Path {
        &self.config_path
    }

    pub fn exists(&self) -> bool {
        self.config_path.exists()
    }

    pub async fn init_default(&self) -> anyhow::Result<Config> {
        let config = Config::default();
        self.save(&config).await?;
        Ok(config)
    }
}

impl Default for ConfigLoader {
    fn default() -> Self {
        Self::new()
    }
}

/// Registry for managing model profiles
#[derive(Debug, Clone, Default)]
pub struct ModelRegistry {
    profiles: HashMap<String, ModelProfile>,
    default_model_id: Option<String>,
}

impl ModelRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_config(config: &AgentsConfig) -> Self {
        let mut registry = Self::default();
        for (id, profile) in &config.models {
            registry.profiles.insert(id.clone(), profile.clone());
        }
        if let Some(ref model) = config.defaults.model {
            registry.default_model_id = Some(model.clone());
        }
        registry
    }

    pub fn get_profile(&self, id: &str) -> Option<&ModelProfile> {
        self.profiles.get(id)
    }

    pub fn get_profile_with_fallback<'a>(
        &'a self,
        id: Option<&'a str>,
    ) -> Option<(&'a str, &'a ModelProfile)> {
        match id {
            Some(id) => self.profiles.get(id).map(|p| (id, p)),
            None => self.get_default_profile(),
        }
    }

    pub fn get_default_profile(&self) -> Option<(&str, &ModelProfile)> {
        self.default_model_id
            .as_ref()
            .and_then(|id| self.profiles.get(id).map(|p| (id.as_str(), p)))
    }

    pub fn get_default_model_id(&self) -> Option<&str> {
        self.default_model_id.as_deref()
    }

    pub fn list_available_models(&self) -> Vec<&str> {
        self.profiles.keys().map(|s| s.as_str()).collect()
    }

    pub fn contains(&self, id: &str) -> bool {
        self.profiles.contains_key(id)
    }

    pub fn len(&self) -> usize {
        self.profiles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.profiles.is_empty()
    }
}

/// Registry for managing LLM provider instances
#[derive(Default)]
pub struct ProviderRegistry {
    configs: HashMap<String, ProviderConfig>,
    default_provider: Option<String>,
    vault: Option<std::sync::Arc<VaultStore>>,
}

impl std::fmt::Debug for ProviderRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderRegistry")
            .field("configs", &self.configs)
            .field("default_provider", &self.default_provider)
            .field("vault", &self.vault.as_ref().map(|_| "VaultStore(..)"))
            .finish()
    }
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_config(config: &Config) -> Self {
        let mut registry = Self::default();
        for (name, provider_config) in &config.providers {
            registry
                .configs
                .insert(name.clone(), provider_config.clone());
        }
        if let Some(ref model) = config.agents.defaults.model {
            let spec: ModelSpec = model.parse().expect("ModelSpec::from_str is infallible");
            if let Some(name) = spec.provider() {
                registry.default_provider = Some(name.to_string());
            }
        }
        registry
    }

    /// Attach an unlocked vault store for JIT secret resolution.
    pub fn with_vault(&mut self, vault: std::sync::Arc<VaultStore>) {
        self.vault = Some(vault);
    }

    /// Resolve a raw API key string through the vault (JIT).
    ///
    /// - No placeholders → returns the string as-is
    /// - Has placeholders & vault available → resolves
    /// - Has placeholders & no vault → error
    fn resolve_api_key(&self, raw: &str) -> anyhow::Result<String> {
        if !contains_placeholders(raw) {
            return Ok(raw.to_string());
        }

        match self.vault.as_ref() {
            Some(v) => v
                .resolve_text(raw)
                .map_err(|e| anyhow::anyhow!("Vault resolution failed: {}", e)),
            None => anyhow::bail!(
                "Config contains vault placeholder(s) but no vault is available. \
                 Set GASKET_VAULT_PASSWORD or run 'gasket vault unlock'."
            ),
        }
    }

    pub fn get_or_create(
        &self,
        name: &str,
    ) -> anyhow::Result<std::sync::Arc<dyn gasket_providers::LlmProvider>> {
        let config = self
            .configs
            .get(name)
            .ok_or_else(|| anyhow::anyhow!("Provider not found: {}", name))?;

        if !config.is_available(name) {
            anyhow::bail!("Provider {} is not available (missing API key)", name);
        }

        let raw_api_key = config.api_key.as_deref().unwrap_or("");
        let api_key = self.resolve_api_key(raw_api_key)?;

        let model = if config.default_model.is_empty() {
            "default".to_string()
        } else {
            config.default_model.clone()
        };

        self.build_provider(name, &api_key, config, &model)
    }

    fn build_provider(
        &self,
        name: &str,
        api_key: &str,
        provider_config: &ProviderConfig,
        model: &str,
    ) -> anyhow::Result<std::sync::Arc<dyn gasket_providers::LlmProvider>> {
        gasket_providers::build_provider(name, api_key, provider_config, model)
    }

    pub fn get_default_provider(&self) -> Option<&str> {
        self.default_provider.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Config validates correctly with no providers.
    #[test]
    fn test_config_validate_empty() {
        let config = Config::default();
        assert!(config.validate().is_ok());
    }

    /// Config detects unavailable provider (missing api_key).
    #[test]
    fn test_config_validate_unavailable_provider() {
        let mut config = Config::default();
        config.providers.insert(
            "test".to_string(),
            ProviderConfig {
                provider_type: ProviderType::Openai,
                api_base: "https://api.example.com".to_string(),
                ..Default::default()
            },
        );
        let result = config.validate();
        assert!(result.is_err());
    }
}
