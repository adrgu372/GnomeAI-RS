//! Provider presets and the small, private settings file used by the TUI.
//!
//! API keys deliberately never enter the session database or the event stream.
//! They live in one owner-only file (`0600`) under `GNOMEF_RS_HOME/store`.
//! Account-backed providers do not store credentials here at all. The bundled
//! Codex app-server and the official Claude Code CLI own their login state.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::codex_app_server::CodexAppServer;
use crate::provider::{Anthropic, CliFlavor, CliProvider, OpenAiCompatible, Provider};
use crate::sandbox::SandboxMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthKind {
    ApiKey,
    Account,
    OptionalApiKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireProtocol {
    OpenAi,
    Anthropic,
    CodexAppServer,
    ClaudeCli,
}

#[derive(Debug, Clone, Copy)]
pub struct ProviderPreset {
    pub id: &'static str,
    pub name: &'static str,
    pub auth: AuthKind,
    pub protocol: WireProtocol,
    pub base_url: &'static str,
    pub default_model: &'static str,
    pub description: &'static str,
}

/// Hosted providers with stable public APIs, plus a catch-all for local and
/// newly launched OpenAI-compatible endpoints.
pub const PROVIDERS: &[ProviderPreset] = &[
    ProviderPreset {
        id: "openai",
        name: "OpenAI API",
        auth: AuthKind::ApiKey,
        protocol: WireProtocol::OpenAi,
        base_url: "https://api.openai.com/v1",
        default_model: "gpt-5.6-terra",
        description: "OpenAI API key",
    },
    ProviderPreset {
        id: "openai-account",
        name: "OpenAI account (Codex)",
        auth: AuthKind::Account,
        protocol: WireProtocol::CodexAppServer,
        base_url: "",
        default_model: "default",
        description: "bundled Codex login; subscription-backed",
    },
    ProviderPreset {
        id: "anthropic",
        name: "Anthropic API",
        auth: AuthKind::ApiKey,
        protocol: WireProtocol::Anthropic,
        base_url: "https://api.anthropic.com/v1",
        default_model: "claude-sonnet-5",
        description: "native Messages API",
    },
    ProviderPreset {
        id: "anthropic-account",
        name: "Anthropic account (Claude Code)",
        auth: AuthKind::Account,
        protocol: WireProtocol::ClaudeCli,
        base_url: "",
        default_model: "default",
        description: "official `claude auth login`; subscription-backed",
    },
    ProviderPreset {
        id: "deepseek",
        name: "DeepSeek",
        auth: AuthKind::ApiKey,
        protocol: WireProtocol::OpenAi,
        base_url: "https://api.deepseek.com",
        default_model: "deepseek-v4-pro",
        description: "DeepSeek platform API",
    },
    ProviderPreset {
        id: "zai-coding",
        name: "Z.ai Coding Plan",
        auth: AuthKind::ApiKey,
        protocol: WireProtocol::OpenAi,
        base_url: "https://api.z.ai/api/coding/paas/v4",
        default_model: "glm-5.3-flash",
        description: "subscription quota via the OpenAI-compatible coding endpoint",
    },
    ProviderPreset {
        id: "moonshot",
        name: "Moonshot / Kimi",
        auth: AuthKind::ApiKey,
        protocol: WireProtocol::OpenAi,
        base_url: "https://api.moonshot.ai/v1",
        default_model: "kimi-k3",
        description: "international Moonshot API",
    },
    ProviderPreset {
        id: "qwen",
        name: "Qwen / DashScope",
        auth: AuthKind::ApiKey,
        protocol: WireProtocol::OpenAi,
        base_url: "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
        default_model: "qwen-plus",
        description: "Alibaba Cloud international endpoint",
    },
    ProviderPreset {
        id: "xai",
        name: "xAI / Grok",
        auth: AuthKind::ApiKey,
        protocol: WireProtocol::OpenAi,
        base_url: "https://api.x.ai/v1",
        default_model: "grok-4.5",
        description: "xAI API",
    },
    ProviderPreset {
        id: "mistral",
        name: "Mistral AI",
        auth: AuthKind::ApiKey,
        protocol: WireProtocol::OpenAi,
        base_url: "https://api.mistral.ai/v1",
        default_model: "mistral-medium-latest",
        description: "Mistral La Plateforme",
    },
    ProviderPreset {
        id: "gemini",
        name: "Google Gemini",
        auth: AuthKind::ApiKey,
        protocol: WireProtocol::OpenAi,
        base_url: "https://generativelanguage.googleapis.com/v1beta/openai",
        default_model: "gemini-3.6-flash",
        description: "Gemini OpenAI-compatible API",
    },
    ProviderPreset {
        id: "groq",
        name: "Groq",
        auth: AuthKind::ApiKey,
        protocol: WireProtocol::OpenAi,
        base_url: "https://api.groq.com/openai/v1",
        default_model: "openai/gpt-oss-120b",
        description: "low-latency inference",
    },
    ProviderPreset {
        id: "openrouter",
        name: "OpenRouter",
        auth: AuthKind::ApiKey,
        protocol: WireProtocol::OpenAi,
        base_url: "https://openrouter.ai/api/v1",
        default_model: "openrouter/auto",
        description: "multi-provider model router",
    },
    ProviderPreset {
        id: "together",
        name: "Together AI",
        auth: AuthKind::ApiKey,
        protocol: WireProtocol::OpenAi,
        base_url: "https://api.together.ai/v1",
        default_model: "MiniMaxAI/MiniMax-M3",
        description: "Together inference API",
    },
    ProviderPreset {
        id: "fireworks",
        name: "Fireworks AI",
        auth: AuthKind::ApiKey,
        protocol: WireProtocol::OpenAi,
        base_url: "https://api.fireworks.ai/inference/v1",
        default_model: "accounts/fireworks/models/deepseek-v3p1",
        description: "Fireworks inference API",
    },
    ProviderPreset {
        id: "perplexity",
        name: "Perplexity",
        auth: AuthKind::ApiKey,
        protocol: WireProtocol::OpenAi,
        base_url: "https://api.perplexity.ai",
        default_model: "sonar-pro",
        description: "search-grounded Sonar API",
    },
    ProviderPreset {
        id: "cerebras",
        name: "Cerebras",
        auth: AuthKind::ApiKey,
        protocol: WireProtocol::OpenAi,
        base_url: "https://api.cerebras.ai/v1",
        default_model: "gpt-oss-120b",
        description: "Cerebras inference API",
    },
    ProviderPreset {
        id: "nvidia",
        name: "NVIDIA NIM",
        auth: AuthKind::ApiKey,
        protocol: WireProtocol::OpenAi,
        base_url: "https://integrate.api.nvidia.com/v1",
        default_model: "openai/gpt-oss-120b",
        description: "NVIDIA hosted NIM endpoint",
    },
    ProviderPreset {
        id: "sambanova",
        name: "SambaNova",
        auth: AuthKind::ApiKey,
        protocol: WireProtocol::OpenAi,
        base_url: "https://api.sambanova.ai/v1",
        default_model: "DeepSeek-V3.1",
        description: "SambaNova Cloud",
    },
    ProviderPreset {
        id: "cohere",
        name: "Cohere",
        auth: AuthKind::ApiKey,
        protocol: WireProtocol::OpenAi,
        base_url: "https://api.cohere.ai/compatibility/v1",
        default_model: "command-a-03-2025",
        description: "Cohere compatibility API",
    },
    ProviderPreset {
        id: "custom",
        name: "Custom / local",
        auth: AuthKind::OptionalApiKey,
        protocol: WireProtocol::OpenAi,
        base_url: "http://127.0.0.1:8090/v1",
        default_model: "local-model",
        description: "any OpenAI-compatible endpoint",
    },
];

pub fn preset(id: &str) -> Option<&'static ProviderPreset> {
    PROVIDERS.iter().find(|provider| provider.id == id)
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ProviderSelection {
    pub provider_id: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    api_key: Option<String>,
}

impl ProviderSelection {
    pub fn from_choice(
        provider_id: impl Into<String>,
        api_key: Option<String>,
        base_url: Option<String>,
    ) -> Result<Self> {
        let provider_id = provider_id.into();
        let selected =
            preset(&provider_id).with_context(|| format!("unknown provider `{provider_id}`"))?;
        let api_key = api_key
            .map(|key| key.trim().to_string())
            .filter(|key| !key.is_empty());
        if selected.auth == AuthKind::ApiKey && api_key.is_none() {
            bail!("{} requires an API key", selected.name);
        }

        let base_url = if provider_id == "custom" {
            let value = base_url
                .as_deref()
                .unwrap_or(selected.base_url)
                .trim()
                .trim_end_matches('/');
            if !(value.starts_with("http://") || value.starts_with("https://")) {
                bail!("custom endpoint must start with http:// or https://");
            }
            Some(value.to_string())
        } else {
            None
        };

        Ok(Self {
            provider_id,
            model: selected.default_model.to_string(),
            base_url,
            api_key,
        })
    }

    pub fn legacy(base_url: String, api_key: Option<String>, model: String) -> Self {
        Self {
            provider_id: "custom".to_string(),
            model,
            base_url: Some(base_url.trim_end_matches('/').to_string()),
            api_key,
        }
    }

    pub fn api_key(&self) -> Option<&str> {
        self.api_key.as_deref()
    }

    pub fn resolved_base_url(&self) -> Option<&str> {
        self.base_url.as_deref().or_else(|| {
            preset(&self.provider_id)
                .map(|provider| provider.base_url)
                .filter(|value| !value.is_empty())
        })
    }

    pub fn protocol_name(&self) -> &'static str {
        match preset(&self.provider_id).map(|provider| provider.protocol) {
            Some(WireProtocol::Anthropic) => "anthropic",
            Some(WireProtocol::CodexAppServer) => "codex",
            Some(WireProtocol::ClaudeCli) => "claude-cli",
            Some(WireProtocol::OpenAi) | None => "openai",
        }
    }
}

#[derive(Clone)]
pub struct ProviderSettingsStore {
    path: PathBuf,
}

impl ProviderSettingsStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn load(&self) -> Result<Option<ProviderSelection>> {
        if !self.path.exists() {
            return Ok(None);
        }
        let metadata = fs::symlink_metadata(&self.path)
            .with_context(|| format!("cannot inspect {}", self.path.display()))?;
        if metadata.file_type().is_symlink() {
            bail!("provider settings must not be a symbolic link");
        }
        if metadata.permissions().mode() & 0o077 != 0 {
            fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600))
                .with_context(|| format!("cannot protect {}", self.path.display()))?;
        }
        let contents = fs::read_to_string(&self.path)
            .with_context(|| format!("cannot read {}", self.path.display()))?;
        let selection: ProviderSelection = serde_json::from_str(&contents)
            .with_context(|| format!("invalid provider settings in {}", self.path.display()))?;
        let selected = preset(&selection.provider_id).with_context(|| {
            format!(
                "provider settings reference unknown provider `{}`",
                selection.provider_id
            )
        })?;
        if selected.auth == AuthKind::ApiKey && selection.api_key.is_none() {
            bail!("saved {} configuration has no API key", selected.name);
        }
        if selection.provider_id == "custom" {
            let base_url = selection.base_url.as_deref().unwrap_or_default();
            if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
                bail!("saved custom provider has an invalid base URL");
            }
        }
        Ok(Some(selection))
    }

    pub fn save(&self, selection: &ProviderSelection) -> Result<()> {
        let parent = self
            .path
            .parent()
            .context("provider settings path has no parent")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
        let tmp = parent.join(format!(".providers-{}.tmp", uuid::Uuid::new_v4().simple()));
        let encoded = serde_json::to_vec_pretty(selection)?;

        let write_result = (|| -> Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)
                .with_context(|| format!("cannot create {}", tmp.display()))?;
            file.write_all(&encoded)?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            fs::rename(&tmp, &self.path)
                .with_context(|| format!("cannot replace {}", self.path.display()))?;
            fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600))?;
            Ok(())
        })();

        if write_result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        write_result
    }
}

pub fn build_provider(
    selection: &ProviderSelection,
    workspace: &Path,
    sandbox: SandboxMode,
) -> Result<Arc<dyn Provider>> {
    let selected = preset(&selection.provider_id)
        .with_context(|| format!("unknown provider `{}`", selection.provider_id))?;
    let provider: Arc<dyn Provider> = match selected.protocol {
        WireProtocol::OpenAi => {
            let base_url = selection
                .base_url
                .as_deref()
                .unwrap_or(selected.base_url)
                .trim_end_matches('/');
            let mut provider =
                OpenAiCompatible::named(selected.name, base_url, selection.api_key.clone());
            provider.use_max_completion_tokens = selected.id == "openai";
            provider.openrouter_credit_fallback = selected.id == "openrouter";
            Arc::new(provider)
        }
        WireProtocol::Anthropic => Arc::new(Anthropic::new(
            selected.name,
            selected.base_url,
            selection
                .api_key
                .clone()
                .context("Anthropic API key is missing")?,
        )),
        WireProtocol::CodexAppServer => {
            Arc::new(CodexAppServer::new(selected.name, workspace, sandbox))
        }
        WireProtocol::ClaudeCli => Arc::new(CliProvider::new(
            selected.name,
            CliFlavor::Claude,
            workspace,
            sandbox,
        )),
    };
    Ok(provider)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_ids_are_unique_and_defaults_are_complete() {
        let mut ids = std::collections::HashSet::new();
        for provider in PROVIDERS {
            assert!(ids.insert(provider.id), "duplicate {}", provider.id);
            assert!(!provider.name.is_empty());
            assert!(!provider.default_model.is_empty());
            if matches!(
                provider.protocol,
                WireProtocol::OpenAi | WireProtocol::Anthropic
            ) {
                assert!(
                    provider.base_url.starts_with("http"),
                    "{} has no endpoint",
                    provider.id
                );
            }
        }
    }

    #[test]
    fn requires_keys_but_allows_keyless_local_endpoint() {
        assert!(ProviderSelection::from_choice("openai", None, None).is_err());
        assert!(ProviderSelection::from_choice("zai-coding", None, None).is_err());
        assert!(
            ProviderSelection::from_choice("custom", None, Some("http://localhost:8080/v1".into()))
                .is_ok()
        );
    }

    #[test]
    fn zai_coding_plan_uses_the_subscription_endpoint_and_flash_default() {
        let selected =
            ProviderSelection::from_choice("zai-coding", Some("zai-key".into()), None).unwrap();

        assert_eq!(selected.protocol_name(), "openai");
        assert_eq!(
            selected.resolved_base_url(),
            Some("https://api.z.ai/api/coding/paas/v4")
        );
        assert_eq!(selected.model, "glm-5.3-flash");
    }

    #[test]
    fn settings_file_is_owner_only() {
        let root = std::env::temp_dir().join(format!(
            "gnomef-provider-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let path = root.join("providers.json");
        let store = ProviderSettingsStore::new(&path);
        let selected =
            ProviderSelection::from_choice("openai", Some("secret".into()), None).unwrap();
        store.save(&selected).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(store.load().unwrap().unwrap().api_key(), Some("secret"));
        fs::remove_dir_all(root).unwrap();
    }
}
