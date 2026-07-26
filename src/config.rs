use std::{
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
    pub llama_base_url: String,
    pub llama_api_mode: String,
    pub llama_timeout: u64,
    pub llama_max_tokens: u32,
    pub tool_loop_max_steps: u32,
    pub agent_max_depth: u32,
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
    pub host: String,
    pub port: u16,
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
            llama_base_url: "http://127.0.0.1:8090/v1".into(),
            llama_api_mode: "chat".into(),
            llama_timeout: 120,
            llama_max_tokens: 4_096,
            tool_loop_max_steps: 6,
            agent_max_depth: 3,
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
            host: "127.0.0.1".into(),
            port: 8787,
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

        let raw = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let mut cfg: Self = serde_json::from_str(&raw).unwrap_or_default();
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
        let raw = serde_json::to_vec_pretty(self)?;
        let result = (|| -> anyhow::Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(&raw)?;
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
        self.provider_protocol = self.provider_protocol.trim().to_lowercase();
        if !matches!(
            self.provider_protocol.as_str(),
            "openai" | "anthropic" | "codex" | "claude-cli"
        ) {
            self.provider_protocol = "openai".into();
        }
        self.firecrawl_api_url = self.firecrawl_api_url.trim().trim_end_matches('/').into();
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
        self.whatsapp_assistant_name = compact_ws(&self.whatsapp_assistant_name);
        self.whatsapp_allowed_jids = self
            .whatsapp_allowed_jids
            .iter()
            .map(|item| compact_ws(item))
            .filter(|item| !item.is_empty())
            .collect();
    }

    pub fn merge_patch(&mut self, patch: &Value) {
        let Some(obj) = patch.as_object() else {
            return;
        };
        let mut value = serde_json::to_value(self.clone()).unwrap_or(Value::Object(Map::new()));
        if let Some(current) = value.as_object_mut() {
            for (key, new_value) in obj {
                if current.contains_key(key) {
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
}
