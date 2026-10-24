//! Agent settings (`shards.brok-agent.settings` in `brok.yml`) and the LLM
//! routing table (the global `llm` section), both handed over by the runtime
//! in the [`brok_sdk::ShardContext`].

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

/// Errors raised while reading settings or resolving a provider.
#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    /// The settings value does not match the expected shape.
    #[error("invalid {section} settings: {source}")]
    Invalid {
        /// Which section failed.
        section: &'static str,
        /// Underlying error.
        #[source]
        source: serde_json::Error,
    },
    /// No provider with that name is configured.
    #[error("unknown LLM provider `{0}` (configure it under llm.providers in brok.yml)")]
    UnknownProvider(String),
    /// No provider is configured at all.
    #[error("no LLM provider configured (add one under llm.providers in brok.yml)")]
    NoProviders,
}

/// Settings of the agent shard.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct AgentSettings {
    /// Seconds between two `git diff` scans.
    pub watch_interval_secs: u64,
    /// Chunks retrieved for each question.
    pub context_chunks: usize,
    /// Upper bound of context characters sent to the model.
    pub max_context_chars: usize,
    /// Pre-indexation review.
    pub review: ReviewSettings,
    /// Replaces the default system prompt.
    pub system_prompt: Option<String>,
}

impl Default for AgentSettings {
    fn default() -> Self {
        Self {
            watch_interval_secs: 5,
            context_chunks: 8,
            max_context_chars: 24_000,
            review: ReviewSettings::default(),
            system_prompt: None,
        }
    }
}

/// Pre-indexation review settings.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct ReviewSettings {
    /// Whether changed files are reviewed before being indexed.
    pub enabled: bool,
    /// Globs of files to review.
    pub include: Vec<String>,
    /// Globs of files never reviewed.
    pub exclude: Vec<String>,
    /// Provider override for reviews.
    pub provider: Option<String>,
    /// Bytes of each file sent for review, in KiB.
    pub max_file_kb: u64,
}

impl Default for ReviewSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            include: vec!["src/api/**".into()],
            exclude: vec!["**/*.md".into()],
            provider: None,
            max_file_kb: 32,
        }
    }
}

impl AgentSettings {
    /// Parses settings; `null` yields the defaults.
    pub fn from_value(value: &Value) -> Result<Self, SettingsError> {
        parse(value, "shards.brok-agent.settings")
    }
}

/// Mirror of the `llm` section of `brok.yml`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct LlmConfig {
    /// Provider used when none is requested.
    pub default: String,
    /// Providers keyed by name.
    pub providers: BTreeMap<String, LlmProvider>,
}

/// An OpenAI-compatible chat completions endpoint.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct LlmProvider {
    /// Base URL up to and including `/v1`.
    pub base_url: String,
    /// Model name.
    pub model: String,
    /// Environment variable holding the API key.
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Sampling temperature.
    #[serde(default)]
    pub temperature: Option<f32>,
    /// Completion token limit.
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

impl LlmConfig {
    /// Parses the routing table; `null` yields an empty table.
    pub fn from_value(value: &Value) -> Result<Self, SettingsError> {
        parse(value, "llm")
    }

    /// Picks `requested`, else the default provider, else the only one.
    pub fn resolve(&self, requested: Option<&str>) -> Result<(&str, &LlmProvider), SettingsError> {
        let name = match requested {
            Some(name) => name,
            None if self.providers.contains_key(&self.default) => self.default.as_str(),
            None if self.providers.len() == 1 => self.providers.keys().next().map(String::as_str).unwrap_or_default(),
            None if self.providers.is_empty() => return Err(SettingsError::NoProviders),
            None => self.default.as_str(),
        };
        self.providers
            .get_key_value(name)
            .map(|(k, v)| (k.as_str(), v))
            .ok_or_else(|| SettingsError::UnknownProvider(name.to_owned()))
    }
}

fn parse<T: Default + for<'de> Deserialize<'de>>(value: &Value, section: &'static str) -> Result<T, SettingsError> {
    if value.is_null() {
        return Ok(T::default());
    }
    T::deserialize(value).map_err(|source| SettingsError::Invalid { section, source })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn null_settings_are_defaults() {
        assert_eq!(AgentSettings::from_value(&Value::Null).unwrap(), AgentSettings::default());
    }

    #[test]
    fn partial_settings_keep_defaults() {
        let s = AgentSettings::from_value(&json!({ "watch_interval_secs": 2, "review": { "enabled": true } })).unwrap();
        assert_eq!(s.watch_interval_secs, 2);
        assert!(s.review.enabled);
        assert_eq!(s.review.include, vec!["src/api/**"]);
        assert_eq!(s.context_chunks, 8);
    }

    #[test]
    fn invalid_settings_are_reported() {
        let err = AgentSettings::from_value(&json!({ "watch_interval_secs": "soon" })).unwrap_err();
        assert!(err.to_string().contains("shards.brok-agent.settings"));
    }

    fn llm() -> LlmConfig {
        LlmConfig::from_value(&json!({
            "default": "ollama",
            "providers": {
                "ollama": { "base_url": "http://localhost:11434/v1", "model": "qwen2.5-coder:7b" },
                "anthropic": { "base_url": "https://api.anthropic.com/v1", "model": "claude", "api_key_env": "ANTHROPIC_API_KEY" }
            }
        }))
        .unwrap()
    }

    #[test]
    fn resolves_providers() {
        let llm = llm();
        assert_eq!(llm.resolve(None).unwrap().0, "ollama");
        let (name, provider) = llm.resolve(Some("anthropic")).unwrap();
        assert_eq!(name, "anthropic");
        assert_eq!(provider.api_key_env.as_deref(), Some("ANTHROPIC_API_KEY"));
        assert!(matches!(llm.resolve(Some("nope")), Err(SettingsError::UnknownProvider(_))));
    }

    #[test]
    fn empty_routing_table() {
        let llm = LlmConfig::from_value(&Value::Null).unwrap();
        assert!(matches!(llm.resolve(None), Err(SettingsError::NoProviders)));
    }
}
