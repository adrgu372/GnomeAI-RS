use std::{fs, path::Path};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
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
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
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
        let raw = serde_json::to_string_pretty(self)?;
        fs::write(path, raw).with_context(|| format!("failed to write {}", path.display()))
    }

    pub fn normalize(&mut self) {
        self.llama_api_mode = self.llama_api_mode.trim().to_lowercase();
        if self.llama_api_mode != "chat" && self.llama_api_mode != "completion" {
            self.llama_api_mode = "chat".into();
        }
        self.llama_base_url = self.llama_base_url.trim().trim_end_matches('/').into();
        self.firecrawl_api_url = self.firecrawl_api_url.trim().trim_end_matches('/').into();
        self.memory_max_facts_in_prompt = self.memory_max_facts_in_prompt.clamp(1, 20);
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
        if let Ok(mut merged) = serde_json::from_value::<Self>(value) {
            merged.normalize();
            *self = merged;
        }
    }
}

pub fn compact_ws(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}
