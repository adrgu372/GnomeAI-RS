//! Model metadata from a vendored [models.dev](https://models.dev) snapshot.
//!
//! The snapshot supplies per-model context/output token limits, so the agent
//! no longer guesses its compaction budget from one hard-coded 128k default
//! and local OpenAI-compatible endpoints (Ollama, llama.cpp, vLLM) can get
//! accurate budgets without maintaining a catalog by hand. The file is
//! `third_party/models.dev/api.json` (MIT, Copyright (c) 2025 models.dev);
//! refresh it with `scripts/fetch-models-dev.sh`.
//!
//! Data here is only a default: `GNOMEF_CONTEXT_WINDOW_TOKENS` keeps winning,
//! unknown providers and models fall back to the hard-coded default, and a
//! malformed or missing snapshot behaves exactly like no snapshot at all.

use serde::Deserialize;
use std::collections::HashMap;
use std::sync::OnceLock;

/// Format of one model entry inside the models.dev `api.json` snapshot.
/// Deserialization is deliberately lenient: anything missing simply means
/// "unknown", and the full entries are far larger than these fields.
#[derive(Debug, Clone, Deserialize)]
struct ModelEntry {
    #[serde(default)]
    limit: Option<ModelLimits>,
}

#[derive(Debug, Clone, Deserialize)]
struct ModelLimits {
    #[serde(default)]
    context: Option<i64>,
    #[serde(default)]
    output: Option<i64>,
}

/// GnomeAI provider preset id -> models.dev provider id. Everything absent
/// here either does not exist upstream (SambaNova) or is a user-defined
/// custom endpoint; both fall back to the hard-coded default budget.
fn models_dev_provider_id(preset_id: &str) -> Option<&str> {
    Some(match preset_id {
        "openai" | "openai-account" => "openai",
        "anthropic" | "anthropic-account" => "anthropic",
        "zai-coding" => "zai-coding-plan",
        "moonshot" => "moonshotai",
        "qwen" => "alibaba",
        "gemini" => "google",
        "together" => "togetherai",
        "fireworks" => "fireworks-ai",
        other => return match other {
            "deepseek" | "xai" | "mistral" | "groq" | "openrouter" | "perplexity"
            | "cerebras" | "nvidia" | "cohere" => Some(other),
            _ => None,
        },
    })
}

type Snapshot = HashMap<String, ProviderEntry>;

#[derive(Debug, Deserialize)]
struct ProviderEntry {
    #[serde(default)]
    models: HashMap<String, ModelEntry>,
}

/// Parse the vendored snapshot once per process. `Ok(None)` (missing file,
/// invalid JSON, no models field) is a normal outcome: the caller then uses
/// the hard-coded default, exactly as before this catalog existed.
fn snapshot() -> Option<&'static Snapshot> {
    static SNAPSHOT: OnceLock<Option<Snapshot>> = OnceLock::new();
    SNAPSHOT
        .get_or_init(|| {
            let raw = include_bytes!("../third_party/models.dev/api.json");
            serde_json::from_slice(raw).ok()
        })
        .as_ref()
}

/// Look up one model in the snapshot. `openrouter` hosts other providers'
/// models under a `vendor/model` id and repeats those entries under their
/// home provider, so a miss there retries with the bare model name.
fn limits_for(preset_id: &str, model: &str) -> Option<ModelLimits> {
    let provider_id = models_dev_provider_id(preset_id)?;
    let provider = snapshot()?.get(provider_id)?;
    let entry = provider
        .models
        .get(model)
        .or_else(|| model.rsplit_once('/').and_then(|(_, bare)| provider.models.get(bare)))?;
    entry.limit.clone()
}

/// Context window (input + output, in tokens) for the given provider preset
/// and model, or `None` when neither the snapshot nor the override knows it.
pub fn context_window_for(preset_id: &str, model: &str) -> Option<i64> {
    limits_for(preset_id, model)?.context
}

/// Maximum completion tokens advertised for the model, when known. Callers
/// decide whether to trust it; the agent loop currently keeps its own
/// request-level cap instead.
pub fn max_output_tokens_for(preset_id: &str, model: &str) -> Option<i64> {
    limits_for(preset_id, model)?.output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_loads_and_knows_the_catalog_defaults() {
        assert_eq!(
            context_window_for("openai", "gpt-5.6-terra"),
            Some(1_050_000)
        );
        assert_eq!(
            context_window_for("zai-coding", "glm-5.3-flash"),
            Some(1_000_000)
        );
        assert_eq!(
            context_window_for("anthropic", "claude-sonnet-5"),
            Some(1_000_000)
        );
    }

    #[test]
    fn aliased_providers_resolve_to_models_dev_ids() {
        assert_eq!(
            context_window_for("qwen", "qwen-plus").filter(|v| *v > 0),
            context_window_for("qwen", "qwen-plus")
        );
        assert!(context_window_for("qwen", "qwen-plus").is_some());
        assert!(context_window_for("gemini", "gemini-3.6-flash").is_some());
    }

    #[test]
    fn openrouter_vendor_prefixed_ids_fall_back_to_the_bare_name() {
        assert!(context_window_for("openrouter", "vendor-that-does-not-exist/x").is_none());
        assert!(max_output_tokens_for("openrouter", "openrouter/auto").is_some());
    }

    #[test]
    fn unknown_providers_and_models_return_none() {
        assert!(context_window_for("custom", "local-model").is_none());
        assert!(context_window_for("sambanova", "DeepSeek-V3.1").is_none());
        assert!(context_window_for("openai", "totally-made-up-model").is_none());
        assert!(max_output_tokens_for("custom", "local-model").is_none());
    }

    #[test]
    fn every_bundled_default_model_with_an_upstream_entry_resolves() {
        // The preset ids of the bundled catalog. Entries without upstream
        // coverage (custom, sambanova) must fall back gracefully; the rest
        // must resolve, so a renamed upstream id cannot rot silently.
        for (preset_id, model, expect_hit) in [
            ("openai", "gpt-5.6-terra", true),
            ("anthropic", "claude-sonnet-5", true),
            ("deepseek", "deepseek-v4-pro", true),
            ("zai-coding", "glm-5.3-flash", true),
            ("moonshot", "kimi-k3", true),
            ("qwen", "qwen-plus", true),
            ("xai", "grok-4.5", true),
            ("mistral", "mistral-medium-latest", true),
            ("gemini", "gemini-3.6-flash", true),
            ("groq", "openai/gpt-oss-120b", true),
            ("openrouter", "openrouter/auto", true),
            ("together", "MiniMaxAI/MiniMax-M3", true),
            // Fireworks renamed their DeepSeek line (now v4); the bundled
            // default id predates the rename and falls back to the 128k
            // default until the preset is refreshed.
            ("fireworks", "accounts/fireworks/models/deepseek-v3p1", false),
            ("perplexity", "sonar-pro", true),
            ("cerebras", "gpt-oss-120b", true),
            ("nvidia", "openai/gpt-oss-120b", true),
            ("cohere", "command-a-03-2025", true),
            ("sambanova", "DeepSeek-V3.1", false),
            ("custom", "local-model", false),
        ] {
            assert_eq!(
                context_window_for(preset_id, model).is_some(),
                expect_hit,
                "{preset_id}/{model}"
            );
        }
    }
}
