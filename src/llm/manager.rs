//! LLM manager for provider credentials and HTTP client.
//!
//! The manager is intentionally simple — it holds API keys, an HTTP client,
//! and shared rate limit state. Routing decisions (which model for which
//! process) live on the agent's RoutingConfig, not here.
//!
//! API keys are hot-reloadable via ArcSwap. The file watcher calls
//! `reload_config()` when config.toml changes, and all subsequent
//! `get_api_key()` calls read the new values lock-free.

use crate::config::LlmConfig;
use crate::error::{LlmError, Result};
use anyhow::Context as _;
use arc_swap::ArcSwap;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::RwLock;

/// Manages LLM provider clients and tracks rate limit state.
pub struct LlmManager {
    config: ArcSwap<LlmConfig>,
    http_client: reqwest::Client,
    /// Models currently in rate limit cooldown, with the time they were limited.
    rate_limited: Arc<RwLock<HashMap<String, Instant>>>,
}

impl LlmManager {
    /// Create a new LLM manager with the given configuration.
    pub async fn new(config: LlmConfig) -> Result<Self> {
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .with_context(|| "failed to build HTTP client")?;

        Ok(Self {
            config: ArcSwap::from_pointee(config),
            http_client,
            rate_limited: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Atomically swap in new provider credentials.
    pub fn reload_config(&self, config: LlmConfig) {
        self.config.store(Arc::new(config));
        tracing::info!("LLM provider keys reloaded");
    }

    /// Get the appropriate API key for a provider.
    pub fn get_api_key(&self, provider: &str) -> Result<String> {
        let config = self.config.load();
        match provider {
            "anthropic" => config.anthropic_key.clone()
                .ok_or_else(|| LlmError::MissingProviderKey("anthropic".into()).into()),
            "openai" => config.openai_key.clone()
                .ok_or_else(|| LlmError::MissingProviderKey("openai".into()).into()),
            "openrouter" => config.openrouter_key.clone()
                .ok_or_else(|| LlmError::MissingProviderKey("openrouter".into()).into()),
            "zhipu" => config.zhipu_key.clone()
                .ok_or_else(|| LlmError::MissingProviderKey("zhipu".into()).into()),
            "groq" => config.groq_key.clone()
                .ok_or_else(|| LlmError::MissingProviderKey("groq".into()).into()),
            "together" => config.together_key.clone()
                .ok_or_else(|| LlmError::MissingProviderKey("together".into()).into()),
            "fireworks" => config.fireworks_key.clone()
                .ok_or_else(|| LlmError::MissingProviderKey("fireworks".into()).into()),
            "deepseek" => config.deepseek_key.clone()
                .ok_or_else(|| LlmError::MissingProviderKey("deepseek".into()).into()),
            "xai" => config.xai_key.clone()
                .ok_or_else(|| LlmError::MissingProviderKey("xai".into()).into()),
            "mistral" => config.mistral_key.clone()
                .ok_or_else(|| LlmError::MissingProviderKey("mistral".into()).into()),
            "ollama" => config.ollama_key.clone()
                .ok_or_else(|| LlmError::MissingProviderKey("ollama".into()).into()),
            "opencode-zen" => config.opencode_zen_key.clone()
                .ok_or_else(|| LlmError::MissingProviderKey("opencode-zen".into()).into()),
            "nvidia" => config.nvidia_key.clone()
                .ok_or_else(|| LlmError::MissingProviderKey("nvidia".into()).into()),
            _ => Err(LlmError::UnknownProvider(provider.into()).into()),
        }
    }

    /// Get configured Ollama base URL, if provided.
    pub fn ollama_base_url(&self) -> Option<String> {
        self.config.load().ollama_base_url.clone()
    }

    /// Get the HTTP client.
    pub fn http_client(&self) -> &reqwest::Client {
        &self.http_client
    }

    /// Resolve a model name to provider and model components.
    /// Format: "provider/model-name" or just "model-name" (defaults to anthropic).
    pub fn resolve_model(&self, model_name: &str) -> Result<(String, String)> {
        if let Some((provider, model)) = model_name.split_once('/') {
            Ok((provider.to_string(), model.to_string()))
        } else {
            Ok(("anthropic".into(), model_name.into()))
        }
    }

    /// Record that a model hit a rate limit.
    pub async fn record_rate_limit(&self, model_name: &str) {
        self.rate_limited.write().await
            .insert(model_name.to_string(), Instant::now());
        tracing::warn!(model = %model_name, "model rate limited, entering cooldown");
    }

    /// Check if a model is currently in rate limit cooldown.
    pub async fn is_rate_limited(&self, model_name: &str, cooldown_secs: u64) -> bool {
        let map = self.rate_limited.read().await;
        if let Some(limited_at) = map.get(model_name) {
            limited_at.elapsed().as_secs() < cooldown_secs
        } else {
            false
        }
    }

    /// Clean up expired rate limit entries.
    pub async fn cleanup_rate_limits(&self, cooldown_secs: u64) {
        self.rate_limited.write().await
            .retain(|_, limited_at| limited_at.elapsed().as_secs() < cooldown_secs);
    }
}
