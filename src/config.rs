use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub provider_id: String,
    pub provider_protocol: String,
    pub web_search_enabled: bool,
    pub firecrawl_api_key: String,
    pub firecrawl_api_url: String,
    pub firecrawl_count: u32,
    pub firecrawl_extract_count: u32,
    pub firecrawl_timeout_ms: u64,
    pub firecrawl_excerpt_chars: usize,
    pub search_ttl: u64,
    pub search_cache_max: usize,
    pub max_chunk_size: usize,
    pub default_model: String,
    pub llama_api_key: String,
    /// Provider-scoped API keys. `llama_api_key` remains the active key for
    /// backwards compatibility, while this map lets switching providers keep
    /// every previously saved credential.
    pub provider_api_keys: BTreeMap<String, String>,
    pub llama_base_url: String,
    pub llama_api_mode: String,
    pub llama_timeout: u64,
    pub llama_max_tokens: u32,
    /// Safety valve for the WebTool tool loop. `0` runs until the model stops
    /// asking for tools, like the terminal agent; the loop is bounded by its
    /// context budget and by the user's interrupt, not by a round count.
    pub tool_loop_max_steps: u32,
    pub agent_max_depth: u32,
    /// Maximum number of local/remote subagents that may execute at once.
    /// This keeps proactive delegation useful without exhausting a local
    /// model server or creating an unbounded number of background workers.
    pub agent_max_concurrent: u32,
    pub remote_agent_api_url: String,
    pub remote_agent_api_key: String,
    pub history_window: usize,
    pub max_messages: usize,
    pub memory_enabled: bool,
    /// Maximum age of cross-chat facts and summaries used in prompts.
    /// `0` keeps all ages eligible.
    pub memory_max_age_days: u32,
    pub memory_max_facts_in_prompt: usize,
    pub memory_max_recent_summaries_in_prompt: usize,
    pub memory_extract_message_window: usize,
    pub memory_max_existing_facts_for_extraction: usize,
    pub memory_max_recent_summaries_stored: usize,
    pub memory_max_facts_stored: usize,
    /// Embedding backend for semantic memory: `auto`, `openai`, `ollama`, `off`.
    /// `auto` picks Ollama when the base URL looks like an Ollama endpoint.
    pub embeddings_provider: String,
    /// Embedding model name. Empty disables embeddings (lexical fallback).
    pub embeddings_model: String,
    /// Embedding endpoint. Empty reuses `llama_base_url` for OpenAI-compatible
    /// providers and `http://127.0.0.1:11434` for Ollama.
    pub embeddings_base_url: String,
    /// Speech-to-text backend for voice notes, audio and video:
    /// `auto` (default) or `off`.
    pub transcription_provider: String,
    /// Transcription model, e.g. `whisper-1`. Empty disables transcription.
    pub transcription_model: String,
    /// Endpoint exposing `/v1/audio/transcriptions`. Empty reuses
    /// `llama_base_url`.
    pub transcription_base_url: String,
    /// Optional ISO-639-1 hint (`ro`, `en`, …). Empty lets the model detect.
    pub transcription_language: String,
    pub memory_dream_enabled: bool,
    /// Minutes of inactivity before an automatic dream cycle may start.
    pub memory_dream_idle_minutes: u32,
    /// Minimum hours between automatic dream cycles.
    pub memory_dream_interval_hours: u32,
    pub memory_dream_max_facts: usize,
    pub memory_dream_max_llm_calls: u32,
    pub memory_dream_max_seconds: u64,
    pub host: String,
    pub port: u16,
    /// Shared execution policy for WebTool and WhatsApp tool calls.
    /// Root access is never implied; the dedicated Sudo tool always asks.
    pub web_sandbox_mode: String,
    pub whatsapp_enabled: bool,
    pub whatsapp_bridge_port: u16,
    pub whatsapp_assistant_name: String,
    pub whatsapp_has_own_number: bool,
    pub whatsapp_allowed_jids: Vec<String>,
    /// Per-process CSRF/API token for the local WebTool. It is injected at
    /// startup and deliberately never serialized to config.json.
    #[serde(skip)]
    pub web_api_token: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            provider_id: "custom".into(),
            provider_protocol: "openai".into(),
            web_search_enabled: false,
            firecrawl_api_key: String::new(),
            firecrawl_api_url: "http://127.0.0.1:3002".into(),
            firecrawl_count: 5,
            firecrawl_extract_count: 3,
            firecrawl_timeout_ms: 30_000,
            firecrawl_excerpt_chars: 2_200,
            search_ttl: 3_600,
            search_cache_max: 500,
            max_chunk_size: 3_500,
            default_model: "gemma-3n-E4B-it-Q4_K_M".into(),
            llama_api_key: String::new(),
            provider_api_keys: BTreeMap::new(),
            llama_base_url: "http://127.0.0.1:8090/v1".into(),
            llama_api_mode: "chat".into(),
            llama_timeout: 120,
            llama_max_tokens: 4_096,
            tool_loop_max_steps: 0,
            agent_max_depth: 3,
            agent_max_concurrent: 4,
            remote_agent_api_url: String::new(),
            remote_agent_api_key: String::new(),
            history_window: 5,
            max_messages: 200,
            memory_enabled: true,
            memory_max_age_days: 90,
            memory_max_facts_in_prompt: 12,
            memory_max_recent_summaries_in_prompt: 3,
            memory_extract_message_window: 12,
            memory_max_existing_facts_for_extraction: 20,
            memory_max_recent_summaries_stored: 120,
            memory_max_facts_stored: 1000,
            embeddings_provider: "auto".into(),
            embeddings_model: String::new(),
            embeddings_base_url: String::new(),
            transcription_provider: "auto".into(),
            transcription_model: String::new(),
            transcription_base_url: String::new(),
            transcription_language: String::new(),
            memory_dream_enabled: true,
            memory_dream_idle_minutes: 5,
            memory_dream_interval_hours: 24,
            memory_dream_max_facts: 400,
            memory_dream_max_llm_calls: 6,
            memory_dream_max_seconds: 120,
            host: "127.0.0.1".into(),
            port: 8787,
            web_sandbox_mode: "normal".into(),
            whatsapp_enabled: false,
            whatsapp_bridge_port: 8788,
            whatsapp_assistant_name: "Gnome AI".into(),
            whatsapp_has_own_number: false,
            whatsapp_allowed_jids: Vec::new(),
            web_api_token: String::new(),
        }
    }
}

impl AppConfig {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }

        let contents = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let mut cfg: Self = serde_json::from_str(&contents).unwrap_or_default();
        cfg.normalize();
        Ok(cfg)
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let parent = path
            .parent()
            .with_context(|| format!("{} has no parent directory", path.display()))?;
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
        let temporary = parent.join(format!(".config-{}.tmp", uuid::Uuid::new_v4().simple()));
        let encoded = serde_json::to_vec_pretty(self)?;
        let result = (|| -> anyhow::Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(&encoded)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            fs::rename(&temporary, path)
                .with_context(|| format!("failed to replace {}", path.display()))?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }

    pub fn normalize(&mut self) {
        self.llama_api_mode = self.llama_api_mode.trim().to_lowercase();
        if self.llama_api_mode != "chat" && self.llama_api_mode != "completion" {
            self.llama_api_mode = "chat".into();
        }
        self.llama_base_url = self.llama_base_url.trim().trim_end_matches('/').into();
        self.provider_id = self.provider_id.trim().to_lowercase();
        if self.provider_id.is_empty() {
            self.provider_id = "custom".into();
        }
        let mut provider_api_keys = std::mem::take(&mut self.provider_api_keys)
            .into_iter()
            .filter_map(|(provider_id, api_key)| {
                let provider_id = provider_id.trim().to_lowercase();
                let api_key = api_key.trim().to_string();
                (!provider_id.is_empty() && !api_key.is_empty()).then_some((provider_id, api_key))
            })
            .collect::<BTreeMap<_, _>>();
        let has_scoped_keys = !provider_api_keys.is_empty();
        let legacy_active_key = self.llama_api_key.trim().to_string();
        if !has_scoped_keys && !legacy_active_key.is_empty() {
            provider_api_keys.insert(self.provider_id.clone(), legacy_active_key);
        }
        self.provider_api_keys = provider_api_keys;
        self.llama_api_key = self
            .provider_api_keys
            .get(&self.provider_id)
            .cloned()
            .unwrap_or_default();
        self.provider_protocol = self.provider_protocol.trim().to_lowercase();
        if !matches!(
            self.provider_protocol.as_str(),
            "openai" | "anthropic" | "codex" | "claude-cli"
        ) {
            self.provider_protocol = "openai".into();
        }
        self.firecrawl_api_url = self.firecrawl_api_url.trim().trim_end_matches('/').into();
        // `0` is meaningful here: it runs the loop until the model stops asking
        // for tools, the way the terminal agent does. Clamping it up to 1 would
        // silently turn the setting into a one-round limit.
        if self.tool_loop_max_steps > 0 {
            self.tool_loop_max_steps = self.tool_loop_max_steps.min(64);
        }
        self.agent_max_depth = self.agent_max_depth.clamp(1, 8);
        self.agent_max_concurrent = self.agent_max_concurrent.clamp(1, 16);
        self.memory_max_facts_in_prompt = self.memory_max_facts_in_prompt.clamp(1, 20);
        if self.memory_max_age_days > 0 {
            self.memory_max_age_days = self.memory_max_age_days.clamp(1, 3_650);
        }
        self.memory_max_recent_summaries_in_prompt =
            self.memory_max_recent_summaries_in_prompt.clamp(0, 10);
        self.memory_extract_message_window = self.memory_extract_message_window.clamp(4, 30);
        self.memory_max_existing_facts_for_extraction =
            self.memory_max_existing_facts_for_extraction.clamp(5, 50);
        self.memory_max_recent_summaries_stored =
            self.memory_max_recent_summaries_stored.clamp(10, 500);
        self.memory_max_facts_stored = self.memory_max_facts_stored.clamp(50, 5000);
        self.embeddings_provider = self.embeddings_provider.trim().to_lowercase();
        if !matches!(
            self.embeddings_provider.as_str(),
            "auto" | "openai" | "ollama" | "off"
        ) {
            self.embeddings_provider = "auto".into();
        }
        self.embeddings_model = compact_ws(&self.embeddings_model);
        self.embeddings_base_url = self.embeddings_base_url.trim().trim_end_matches('/').into();
        self.transcription_provider = self.transcription_provider.trim().to_lowercase();
        if !matches!(
            self.transcription_provider.as_str(),
            "auto" | "openai" | "off"
        ) {
            self.transcription_provider = "auto".into();
        }
        self.transcription_model = compact_ws(&self.transcription_model);
        self.transcription_base_url = self
            .transcription_base_url
            .trim()
            .trim_end_matches('/')
            .into();
        self.transcription_language = self.transcription_language.trim().to_lowercase();
        self.memory_dream_idle_minutes = self.memory_dream_idle_minutes.clamp(5, 24 * 60);
        self.memory_dream_interval_hours = self.memory_dream_interval_hours.clamp(1, 24 * 7);
        self.memory_dream_max_facts = self.memory_dream_max_facts.clamp(20, 5000);
        self.memory_dream_max_llm_calls = self.memory_dream_max_llm_calls.min(20);
        self.memory_dream_max_seconds = self.memory_dream_max_seconds.clamp(10, 900);
        self.web_sandbox_mode = self.web_sandbox_mode.trim().to_ascii_lowercase();
        if !matches!(
            self.web_sandbox_mode.as_str(),
            "read-only" | "normal" | "full-access"
        ) {
            self.web_sandbox_mode = "normal".into();
        }
        self.whatsapp_assistant_name = compact_ws(&self.whatsapp_assistant_name);
        self.whatsapp_allowed_jids = self
            .whatsapp_allowed_jids
            .iter()
            .map(|item| compact_ws(item))
            .filter(|item| !item.is_empty())
            .collect();
    }

    pub fn provider_api_key(&self, provider_id: &str) -> Option<&str> {
        self.provider_api_keys
            .get(&provider_id.trim().to_lowercase())
            .map(String::as_str)
            .filter(|key| !key.is_empty())
    }

    /// Prefer a newly supplied key, otherwise reuse the credential remembered
    /// for this provider. The legacy active key is a final migration fallback.
    pub fn resolve_provider_api_key(
        &self,
        provider_id: &str,
        supplied: Option<String>,
    ) -> Option<String> {
        supplied
            .map(|key| key.trim().to_string())
            .filter(|key| !key.is_empty())
            .or_else(|| self.provider_api_key(provider_id).map(str::to_string))
            .or_else(|| {
                (self.provider_id == provider_id && !self.llama_api_key.trim().is_empty())
                    .then(|| self.llama_api_key.trim().to_string())
            })
    }

    pub fn remember_provider_api_key(&mut self, provider_id: &str, api_key: Option<&str>) {
        let provider_id = provider_id.trim().to_lowercase();
        let api_key = api_key.map(str::trim).filter(|key| !key.is_empty());
        if !provider_id.is_empty() {
            if let Some(api_key) = api_key {
                self.provider_api_keys
                    .insert(provider_id, api_key.to_string());
            }
        }
    }

    pub fn merge_patch(&mut self, patch: &Value) {
        let Some(obj) = patch.as_object() else {
            return;
        };
        let mut value = serde_json::to_value(self.clone()).unwrap_or(Value::Object(Map::new()));
        if let Some(current) = value.as_object_mut() {
            for (key, new_value) in obj {
                // Provider credentials are changed only through the dedicated
                // provider endpoint, never through a generic JSON merge.
                if !matches!(key.as_str(), "provider_api_keys" | "llama_api_key")
                    && current.contains_key(key)
                {
                    current.insert(key.clone(), new_value.clone());
                }
            }
        }
        let web_api_token = self.web_api_token.clone();
        if let Ok(mut merged) = serde_json::from_value::<Self>(value) {
            merged.normalize();
            merged.web_api_token = web_api_token;
            *self = merged;
        }
    }
}

pub fn compact_ws(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn runtime_web_token_is_neither_serialized_nor_lost_by_patch() {
        let mut config = AppConfig::default();
        config.web_api_token = "runtime-secret".into();
        assert!(
            !serde_json::to_string(&config)
                .unwrap()
                .contains("runtime-secret")
        );

        config.merge_patch(&json!({"default_model": "new-model"}));
        assert_eq!(config.default_model, "new-model");
        assert_eq!(config.web_api_token, "runtime-secret");
    }

    #[test]
    fn provider_keys_survive_switches_and_legacy_config_is_migrated() {
        let mut config = AppConfig::default();
        config.provider_id = "openai".into();
        config.llama_api_key = "sk-openai".into();
        config.normalize();
        assert_eq!(config.provider_api_key("openai"), Some("sk-openai"));

        config.remember_provider_api_key("anthropic", Some("sk-anthropic"));
        config.provider_id = "anthropic".into();
        config.llama_api_key = config.provider_api_key("anthropic").unwrap().to_string();
        assert_eq!(config.llama_api_key, "sk-anthropic");
        assert_eq!(
            config.resolve_provider_api_key("openai", None),
            Some("sk-openai".into())
        );

        config.merge_patch(&json!({"provider_api_keys": {"openai": "stolen"}}));
        assert_eq!(config.provider_api_key("openai"), Some("sk-openai"));
    }

    #[test]
    fn provider_keys_round_trip_in_owner_only_config() {
        let root = std::env::temp_dir().join(format!(
            "gnomef-provider-keys-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let path = root.join("config.json");
        let mut config = AppConfig::default();
        config.provider_id = "anthropic".into();
        config.remember_provider_api_key("openai", Some("sk-openai"));
        config.remember_provider_api_key("anthropic", Some("sk-anthropic"));
        config.llama_api_key = "sk-anthropic".into();
        config.save(&path).unwrap();

        let loaded = AppConfig::load(&path).unwrap();
        assert_eq!(loaded.provider_api_key("openai"), Some("sk-openai"));
        assert_eq!(loaded.provider_api_key("anthropic"), Some("sk-anthropic"));
        assert_eq!(loaded.llama_api_key, "sk-anthropic");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shared_web_sandbox_mode_is_normalized() {
        let mut config = AppConfig::default();
        config.web_sandbox_mode = " FULL-ACCESS ".into();
        config.normalize();
        assert_eq!(config.web_sandbox_mode, "full-access");

        config.web_sandbox_mode = "unknown".into();
        config.normalize();
        assert_eq!(config.web_sandbox_mode, "normal");
    }
}
