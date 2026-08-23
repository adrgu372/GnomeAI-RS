use std::collections::HashMap;
use std::pin::Pin;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use async_stream::try_stream;
use futures_util::{Stream, StreamExt};
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde_json::{Value, json};

use crate::config::AppConfig;

#[derive(Clone)]
pub struct LlamaClient {
    http: reqwest::Client,
}

#[derive(Debug, Clone)]
pub struct LlamaResponse {
    pub content: String,
    pub tool_calls: Vec<Value>,
}

#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub id: String,
    pub capabilities: Vec<String>,
}

/// Known models per provider, used when the provider has no public `/models`
/// endpoint (Anthropic) or as a fallback when the endpoint is unreachable.
pub fn known_models(provider_id: &str) -> Vec<ModelInfo> {
    match provider_id {
        "openai" => vec![
            model("gpt-5.6-terra"),
            model("gpt-5.4"),
            model("gpt-5.3"),
            model("gpt-4o"),
            model("gpt-4o-mini"),
        ],
        "anthropic" => vec![
            model("claude-sonnet-5"),
            model("claude-opus-4-8"),
            model("claude-opus-4-7"),
            model("claude-haiku-3-5"),
        ],
        "deepseek" => vec![
            model("deepseek-v4-pro"),
            model("deepseek-v4"),
            model("deepseek-v3"),
            model("deepseek-r1"),
        ],
        "moonshot" => vec![model("kimi-k3"), model("kimi-k2.5")],
        "qwen" => vec![
            model("qwen-plus"),
            model("qwen-max"),
            model("qwen-turbo"),
            model("qwen-coder-plus"),
        ],
        "xai" => vec![model("grok-4.5"), model("grok-4"), model("grok-3")],
        "mistral" => vec![
            model("mistral-medium-latest"),
            model("mistral-small-latest"),
            model("pixtral-large-latest"),
            model("codestral-latest"),
        ],
        "gemini" => vec![
            model("gemini-3.6-flash"),
            model("gemini-3.6-pro"),
            model("gemini-3.0-flash"),
            model("gemini-3.0-pro"),
        ],
        "groq" => vec![
            model("openai/gpt-oss-120b"),
            model("meta-llama/llama-4.5-maverick"),
            model("qwen-3.5"),
        ],
        "openrouter" => vec![model("openrouter/auto")],
        "together" => vec![model("MiniMaxAI/MiniMax-M3")],
        "fireworks" => vec![model("accounts/fireworks/models/deepseek-v3p1")],
        "perplexity" => vec![model("sonar-pro"), model("sonar-deep-research")],
        "cerebras" => vec![model("gpt-oss-120b"), model("llama-4.5-maverick")],
        "nvidia" => vec![model("openai/gpt-oss-120b")],
        "sambanova" => vec![model("DeepSeek-V3.1"), model("DeepSeek-R1")],
        "cohere" => vec![model("command-a-03-2025"), model("command-r-plus")],
        // A ChatGPT account can expose a different Codex catalog depending on
        // its plan and rollout. Never guess those ids: model/list replaces
        // this safe fallback whenever the account runtime is available.
        "openai-account" => vec![model("default")],
        // Claude Code accepts these stable aliases and resolves them to the
        // newest model included in the connected Anthropic subscription.
        "anthropic-account" => vec![
            model("default"),
            model("sonnet"),
            model("opus"),
            model("haiku"),
        ],
        "custom" | _ => Vec::new(),
    }
}

/// Convert provider model metadata into a stable list for user interfaces.
/// The active model is always present and shown first, even when an account
/// provider cannot expose a model-list endpoint or the API omits it.
pub fn model_ids(models: Vec<ModelInfo>, active_model: &str) -> Vec<String> {
    let ids = models
        .into_iter()
        .map(|model| model.id.trim().to_string())
        .collect::<Vec<_>>();
    normalize_model_ids(ids, active_model)
}

pub fn normalize_model_ids(mut ids: Vec<String>, active_model: &str) -> Vec<String> {
    ids = ids
        .into_iter()
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
        .collect();
    ids.sort_unstable();
    ids.dedup();

    let active_model = active_model.trim();
    if !active_model.is_empty() {
        if let Some(index) = ids.iter().position(|id| id == active_model) {
            ids.remove(index);
        }
        ids.insert(0, active_model.to_string());
    }
    ids
}

/// Normalize Codex account models without reviving a stale or guessed model.
/// `default` remains usable when discovery is temporarily unavailable; an
/// explicit model is selected only when model/list actually advertised it.
pub fn codex_account_model_ids(models: Vec<ModelInfo>, active_model: &str) -> Vec<String> {
    let mut ids = model_ids(models, "");
    if !ids.iter().any(|model| model == "default") {
        ids.push("default".to_string());
    }
    let active_model = active_model.trim();
    let selected = if !active_model.is_empty() && ids.iter().any(|model| model == active_model) {
        active_model
    } else {
        "default"
    };
    normalize_model_ids(ids, selected)
}

fn model(id: &str) -> ModelInfo {
    ModelInfo {
        id: id.to_string(),
        capabilities: vec![],
    }
}

pub type TokenStream = Pin<Box<dyn Stream<Item = anyhow::Result<String>> + Send>>;

/// One piece of a streamed model round.
///
/// Text and reasoning arrive as deltas so an interface can draw them while the
/// model is still writing. Tool calls deliberately do not: a half-assembled
/// argument object is not something the tool loop can act on, so fragments are
/// joined here and emitted once, complete.
#[derive(Debug, Clone)]
pub enum ChatStreamEvent {
    Text(String),
    Reasoning(String),
    /// Emitted at most once, after the upstream stream ends.
    ToolCalls(Vec<Value>),
}

pub type ChatEventStream = Pin<Box<dyn Stream<Item = anyhow::Result<ChatStreamEvent>> + Send>>;

impl LlamaClient {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }

    /// The shared connection pool, for sibling endpoints on the same provider
    /// (embeddings, transcription).
    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    pub async fn list_models(&self, cfg: &AppConfig) -> anyhow::Result<Vec<ModelInfo>> {
        if cfg.provider_id == "openai-account" {
            let discovered = match tokio::time::timeout(
                Duration::from_secs(20),
                crate::codex_app_server::list_available_models(),
            )
            .await
            {
                Ok(Ok(models)) if models.len() > 1 => Some(models),
                Ok(Ok(_)) => {
                    tracing::warn!("Codex model/list returned no selectable account models");
                    None
                }
                Ok(Err(error)) => {
                    tracing::warn!(%error, "Codex model discovery failed; using default");
                    None
                }
                Err(_) => {
                    tracing::warn!("Codex model discovery timed out; using default");
                    None
                }
            };
            return Ok(discovered
                .map(|models| models.into_iter().map(|id| model(&id)).collect())
                .unwrap_or_else(|| known_models("openai-account")));
        }
        if cfg.provider_id == "anthropic-account" {
            return Ok(known_models("anthropic-account"));
        }
        if cfg.provider_protocol == "anthropic" {
            return Ok(known_models("anthropic"));
        }
        ensure_web_provider_supported(cfg)?;
        for url in candidate_model_urls(cfg) {
            let result = self
                .http
                .get(&url)
                .headers(headers(cfg)?)
                .timeout(Duration::from_secs(8))
                .send()
                .await;
            let response = match result {
                Ok(response) => response,
                Err(_) => continue,
            };
            if response.status().as_u16() == 404 {
                continue;
            }
            if !response.status().is_success() {
                // Consume the response so the pooled connection can be reused.
                let _ = response.bytes().await;
                continue;
            }
            let payload: Value = match response.json().await {
                Ok(payload) => payload,
                Err(_) => continue,
            };
            let models = parse_models(&payload);
            if !models.is_empty() {
                return Ok(models);
            }
        }
        Ok(known_models(&cfg.provider_id))
    }

    async fn retry_openrouter_free(
        &self,
        cfg: &AppConfig,
        url: &str,
        payload: &mut Value,
        response: reqwest::Response,
        needs_tools: bool,
        needs_images: bool,
    ) -> anyhow::Result<reqwest::Response> {
        if cfg.provider_id != "openrouter"
            || !crate::openrouter::is_credit_exhausted(response.status())
        {
            return Ok(response);
        }

        let _ = response.bytes().await;
        let models = crate::openrouter::ranked_free_models(
            &self.http,
            (!cfg.llama_api_key.trim().is_empty()).then_some(cfg.llama_api_key.as_str()),
            needs_tools,
            needs_images,
        )
        .await
        .unwrap_or_else(|error| {
            tracing::warn!(%error, "OpenRouter free-model discovery failed; using free router");
            crate::openrouter::free_router_only()
        });
        crate::openrouter::apply_free_model_fallback(payload, &models);
        tracing::warn!(
            first_candidate = %models.first().map(String::as_str).unwrap_or("openrouter/free"),
            "OpenRouter credits exhausted; retrying with ranked free models"
        );
        self.http
            .post(url)
            .headers(headers(cfg)?)
            .json(payload)
            .send()
            .await
            .context("OpenRouter free fallback request failed")
    }

    pub async fn chat(
        &self,
        cfg: &AppConfig,
        model: &str,
        messages: Vec<Value>,
        temperature: f64,
    ) -> anyhow::Result<LlamaResponse> {
        self.chat_inner(cfg, model, messages, temperature, None, None)
            .await
    }

    pub async fn chat_with_tools(
        &self,
        cfg: &AppConfig,
        model: &str,
        messages: Vec<Value>,
        temperature: f64,
        tools: Vec<Value>,
        tool_choice: Option<Value>,
    ) -> anyhow::Result<LlamaResponse> {
        self.chat_inner(cfg, model, messages, temperature, Some(tools), tool_choice)
            .await
    }

    pub async fn chat_stream(
        &self,
        cfg: &AppConfig,
        model: &str,
        messages: Vec<Value>,
        temperature: f64,
    ) -> anyhow::Result<TokenStream> {
        if cfg.provider_protocol == "anthropic" {
            return self
                .anthropic_chat_stream(cfg, model, messages, temperature)
                .await;
        }
        ensure_web_provider_supported(cfg)?;
        let has_images = messages_have_images(&messages);
        let mut last_error = None;
        let mut attempted = false;

        for url in candidate_chat_urls(cfg) {
            let is_completion = url.ends_with("/completion");
            if has_images && is_completion {
                continue;
            }
            attempted = true;

            let mut payload =
                if (is_completion || cfg.llama_api_mode == "completion") && !has_images {
                    json!({
                        "prompt": prompt_from_messages(&messages),
                        "temperature": temperature,
                        "n_predict": cfg.llama_max_tokens,
                        "cache_prompt": true,
                        "stream": true,
                    })
                } else {
                    let mut payload = json!({
                        "model": model,
                        "messages": messages,
                        "temperature": temperature,
                        "stream": true,
                    });
                    set_openai_token_limit(cfg, &mut payload);
                    payload
                };

            let response = self
                .http
                .post(&url)
                .headers(headers(cfg)?)
                .json(&payload)
                .send()
                .await;
            let response = match response {
                Ok(response) => response,
                Err(err) => {
                    last_error = Some(err.into());
                    continue;
                }
            };
            let response = if is_completion {
                response
            } else {
                match self
                    .retry_openrouter_free(cfg, &url, &mut payload, response, false, has_images)
                    .await
                {
                    Ok(response) => response,
                    Err(err) => {
                        last_error = Some(err);
                        continue;
                    }
                }
            };
            if response.status().as_u16() == 404 {
                continue;
            }
            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                last_error = Some(anyhow!(
                    "{url} returned HTTP {status}: {}",
                    body.chars().take(800).collect::<String>()
                ));
                continue;
            }

            return Ok(Box::pin(stream_tokens_from_response(response)));
        }

        if has_images && !attempted {
            bail!(
                "image input requires a chat/completions endpoint; configured base URL only offered completion endpoints"
            );
        }
        Err(last_error.unwrap_or_else(|| anyhow!("llama-server streaming request failed")))
    }

    /// Streaming counterpart of [`Self::chat_with_tools`].
    ///
    /// Returns an error for anything that cannot carry OpenAI-style streaming
    /// tool calls — the Anthropic wire protocol, and endpoints that only expose
    /// the bare `/completion` route. Callers are expected to fall back to the
    /// buffered `chat_with_tools`, so no provider loses functionality by this
    /// method existing.
    pub async fn chat_stream_with_tools(
        &self,
        cfg: &AppConfig,
        model: &str,
        messages: Vec<Value>,
        temperature: f64,
        tools: Vec<Value>,
        tool_choice: Option<Value>,
    ) -> anyhow::Result<ChatEventStream> {
        // Checked before the protocol split so both wire formats agree: vision
        // turns never reach the tool loop, they take the buffered path.
        if messages_have_images(&messages) {
            bail!("image input uses the buffered vision path");
        }
        if cfg.provider_protocol == "anthropic" {
            return self
                .anthropic_chat_stream_with_tools(cfg, model, messages, temperature, tools)
                .await;
        }
        ensure_web_provider_supported(cfg)?;
        let needs_tools = !tools.is_empty();
        let mut last_error = None;

        for url in candidate_chat_urls(cfg) {
            // Tool calling only exists on the chat route.
            if url.ends_with("/completion") {
                continue;
            }

            let mut payload = json!({
                "model": model,
                "messages": messages,
                "temperature": temperature,
                "stream": true,
            });
            set_openai_token_limit(cfg, &mut payload);
            if needs_tools {
                payload["tools"] = json!(tools);
                if let Some(choice) = tool_choice.clone() {
                    payload["tool_choice"] = choice;
                }
            }

            let response = self
                .http
                .post(&url)
                .headers(headers(cfg)?)
                .json(&payload)
                .send()
                .await;
            let response = match response {
                Ok(response) => response,
                Err(err) => {
                    last_error = Some(err.into());
                    continue;
                }
            };
            let response = match self
                .retry_openrouter_free(cfg, &url, &mut payload, response, needs_tools, false)
                .await
            {
                Ok(response) => response,
                Err(err) => {
                    last_error = Some(err);
                    continue;
                }
            };
            if response.status().as_u16() == 404 {
                continue;
            }
            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                last_error = Some(anyhow!(
                    "{url} returned HTTP {status}: {}",
                    body.chars().take(800).collect::<String>()
                ));
                continue;
            }

            return Ok(Box::pin(stream_events_from_response(response)));
        }

        Err(last_error
            .unwrap_or_else(|| anyhow!("no streaming chat endpoint accepted the tool request")))
    }

    async fn chat_inner(
        &self,
        cfg: &AppConfig,
        model: &str,
        messages: Vec<Value>,
        temperature: f64,
        tools: Option<Vec<Value>>,
        tool_choice: Option<Value>,
    ) -> anyhow::Result<LlamaResponse> {
        if cfg.provider_protocol == "anthropic" {
            return self
                .anthropic_chat(cfg, model, messages, temperature, tools)
                .await;
        }
        ensure_web_provider_supported(cfg)?;
        let has_images = messages_have_images(&messages);
        let has_tools = tools.as_ref().is_some_and(|items| !items.is_empty());
        let mut last_error = None;
        let mut attempted = false;

        for url in candidate_chat_urls(cfg) {
            let is_completion = url.ends_with("/completion");
            if (has_images || has_tools) && is_completion {
                continue;
            }
            attempted = true;

            let mut payload = if (is_completion || cfg.llama_api_mode == "completion")
                && !has_images
                && !has_tools
            {
                json!({
                    "prompt": prompt_from_messages(&messages),
                    "temperature": temperature,
                    "n_predict": cfg.llama_max_tokens,
                    "cache_prompt": true,
                    "stream": false,
                })
            } else {
                let mut payload = json!({
                    "model": model,
                    "messages": messages,
                    "temperature": temperature,
                    "stream": false,
                });
                set_openai_token_limit(cfg, &mut payload);
                if let Some(tools) = tools.clone() {
                    payload["tools"] = Value::Array(tools);
                    if let Some(tool_choice) = tool_choice.clone() {
                        payload["tool_choice"] = tool_choice;
                    }
                }
                payload
            };

            let response = self
                .http
                .post(&url)
                .headers(headers(cfg)?)
                .json(&payload)
                .send()
                .await;
            let response = match response {
                Ok(response) => response,
                Err(err) => {
                    last_error = Some(err.into());
                    continue;
                }
            };

            let response = if is_completion {
                response
            } else {
                match self
                    .retry_openrouter_free(cfg, &url, &mut payload, response, has_tools, has_images)
                    .await
                {
                    Ok(response) => response,
                    Err(err) => {
                        last_error = Some(err);
                        continue;
                    }
                }
            };

            if response.status().as_u16() == 404 {
                continue;
            }
            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                last_error = Some(anyhow!(
                    "{url} returned HTTP {status}: {}",
                    body.chars().take(800).collect::<String>()
                ));
                continue;
            }

            let payload: Value = response
                .json()
                .await
                .with_context(|| format!("failed to parse llama response from {url}"))?;
            return Ok(LlamaResponse {
                content: extract_response_text(&payload),
                tool_calls: extract_tool_calls(&payload),
            });
        }

        if has_images && !attempted {
            return Err(anyhow!(
                "image input requires a chat/completions endpoint; configured base URL only offered completion endpoints"
            ));
        }
        if has_tools && !attempted {
            return Err(anyhow!(
                "tool input requires a chat/completions endpoint; configured base URL only offered completion endpoints"
            ));
        }
        Err(last_error.unwrap_or_else(|| anyhow!("llama-server request failed")))
    }

    async fn anthropic_chat(
        &self,
        cfg: &AppConfig,
        model: &str,
        messages: Vec<Value>,
        temperature: f64,
        tools: Option<Vec<Value>>,
    ) -> anyhow::Result<LlamaResponse> {
        let (system, messages) = anthropic_messages(&messages);
        let mut payload = json!({
            "model": model,
            "messages": messages,
            "temperature": temperature,
            "max_tokens": cfg.llama_max_tokens,
            "stream": false,
        });
        if !system.is_empty() {
            payload["system"] = json!(system);
        }
        if let Some(tools) = tools {
            let tools = tools
                .iter()
                .filter_map(anthropic_tool_schema)
                .collect::<Vec<_>>();
            if !tools.is_empty() {
                payload["tools"] = Value::Array(tools);
            }
        }

        let url = anthropic_messages_url(cfg);
        let response = self
            .http
            .post(&url)
            .headers(anthropic_headers(cfg)?)
            .json(&payload)
            .send()
            .await
            .context("Anthropic request failed")?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!(
                "Anthropic returned {status}: {}",
                body.chars().take(800).collect::<String>()
            );
        }
        let response: Value = response
            .json()
            .await
            .context("failed to parse Anthropic response")?;
        Ok(anthropic_response(&response))
    }

    async fn anthropic_chat_stream(
        &self,
        cfg: &AppConfig,
        model: &str,
        messages: Vec<Value>,
        temperature: f64,
    ) -> anyhow::Result<TokenStream> {
        let (system, messages) = anthropic_messages(&messages);
        let mut payload = json!({
            "model": model,
            "messages": messages,
            "temperature": temperature,
            "max_tokens": cfg.llama_max_tokens,
            "stream": true,
        });
        if !system.is_empty() {
            payload["system"] = json!(system);
        }
        let url = anthropic_messages_url(cfg);
        let response = self
            .http
            .post(&url)
            .headers(anthropic_headers(cfg)?)
            .json(&payload)
            .send()
            .await
            .context("Anthropic streaming request failed")?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!(
                "Anthropic returned {status}: {}",
                body.chars().take(800).collect::<String>()
            );
        }
        Ok(Box::pin(stream_anthropic_tokens(response)))
    }

    /// Anthropic's own streaming format, carrying tool calls.
    ///
    /// Shares the request shape with [`Self::anthropic_chat`] so a streamed
    /// round and a buffered one cannot drift into sending different tools.
    async fn anthropic_chat_stream_with_tools(
        &self,
        cfg: &AppConfig,
        model: &str,
        messages: Vec<Value>,
        temperature: f64,
        tools: Vec<Value>,
    ) -> anyhow::Result<ChatEventStream> {
        let (system, messages) = anthropic_messages(&messages);
        let mut payload = json!({
            "model": model,
            "messages": messages,
            "temperature": temperature,
            "max_tokens": cfg.llama_max_tokens,
            "stream": true,
        });
        if !system.is_empty() {
            payload["system"] = json!(system);
        }
        let tools = tools
            .iter()
            .filter_map(anthropic_tool_schema)
            .collect::<Vec<_>>();
        if !tools.is_empty() {
            payload["tools"] = Value::Array(tools);
        }

        let response = self
            .http
            .post(&anthropic_messages_url(cfg))
            .headers(anthropic_headers(cfg)?)
            .json(&payload)
            .send()
            .await
            .context("Anthropic streaming request failed")?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!(
                "Anthropic returned {status}: {}",
                body.chars().take(800).collect::<String>()
            );
        }
        Ok(Box::pin(stream_anthropic_events_from_response(response)))
    }
}

fn ensure_web_provider_supported(cfg: &AppConfig) -> anyhow::Result<()> {
    match cfg.provider_protocol.as_str() {
        "codex" => bail!("OpenAI account requests must use the Codex app-server adapter"),
        "claude-cli" => bail!("Anthropic account requests must use the Claude Code adapter"),
        _ => Ok(()),
    }
}

fn set_openai_token_limit(cfg: &AppConfig, payload: &mut Value) {
    let key = if cfg.provider_id == "openai" {
        "max_completion_tokens"
    } else {
        "max_tokens"
    };
    payload[key] = json!(cfg.llama_max_tokens);
}

/// Read an OpenAI-style SSE response into text deltas plus one assembled batch
/// of tool calls.
///
/// SSE frames are separated by a blank line; anything short of that is a
/// partial frame and stays in the buffer. Tool-call fragments key off `index`
/// because that is the only field repeated on every fragment — `id` and `name`
/// arrive once and then never again.
fn stream_events_from_response(
    response: reqwest::Response,
) -> impl Stream<Item = anyhow::Result<ChatStreamEvent>> + Send {
    try_stream! {
        let mut chunks = response.bytes_stream();
        let mut buffer = String::new();
        let mut assembler = ToolCallAssembler::default();

        'outer: while let Some(chunk) = chunks.next().await {
            buffer.push_str(&String::from_utf8_lossy(&chunk?));

            while let Some((position, delimiter)) = sse_frame_boundary(&buffer) {
                let frame: String = buffer.drain(..position + delimiter).collect();

                for line in frame.lines() {
                    let line = line.trim();
                    if line.is_empty() || line.starts_with(':') {
                        continue;
                    }
                    let data = line.strip_prefix("data:").map(str::trim).unwrap_or(line);
                    if data.is_empty() {
                        continue;
                    }
                    if data == "[DONE]" {
                        break 'outer;
                    }
                    // A malformed frame is not worth aborting a live answer for.
                    let Ok(payload) = serde_json::from_str::<Value>(data) else {
                        continue;
                    };

                    for token in extract_stream_tokens(&payload) {
                        if !token.is_empty() {
                            yield ChatStreamEvent::Text(token);
                        }
                    }

                    let Some(choice) = payload["choices"].get(0) else {
                        continue;
                    };
                    let delta = &choice["delta"];

                    // Reasoning models expose thinking under different keys.
                    for key in ["reasoning_content", "reasoning"] {
                        if let Some(text) = delta[key].as_str() {
                            if !text.is_empty() {
                                yield ChatStreamEvent::Reasoning(text.to_string());
                            }
                        }
                    }

                    assembler.absorb(delta);
                }
            }
        }

        let calls = assembler.finish();
        if !calls.is_empty() {
            yield ChatStreamEvent::ToolCalls(calls);
        }
    }
}

/// Joins streamed `tool_calls` fragments back into whole calls.
///
/// `index` is the only field repeated on every fragment; `id` and `name` arrive
/// once and then never again, and `arguments` is a JSON string split across an
/// arbitrary number of chunks. Anything that assumes otherwise silently
/// produces calls with truncated arguments.
#[derive(Default)]
struct ToolCallAssembler {
    pending: HashMap<u64, PartialToolCall>,
}

impl ToolCallAssembler {
    fn absorb(&mut self, delta: &Value) {
        let Some(calls) = delta["tool_calls"].as_array() else {
            return;
        };
        for call in calls {
            let index = call["index"].as_u64().unwrap_or(0);
            let entry = self.pending.entry(index).or_default();
            if let Some(id) = call["id"].as_str() {
                if !id.is_empty() {
                    entry.id = id.to_string();
                }
            }
            if let Some(name) = call["function"]["name"].as_str() {
                if !name.is_empty() {
                    entry.name = name.to_string();
                }
            }
            if let Some(arguments) = call["function"]["arguments"].as_str() {
                entry.arguments.push_str(arguments);
            }
        }
    }

    /// Anthropic announces a tool call up front with its id and name, then
    /// streams the arguments separately as `input_json_delta` fragments.
    fn start_block(&mut self, index: u64, id: &str, name: &str, input: &Value) {
        let entry = self.pending.entry(index).or_default();
        if !id.is_empty() {
            entry.id = id.to_string();
        }
        if !name.is_empty() {
            entry.name = name.to_string();
        }
        // A non-empty `input` here means the whole argument object arrived at
        // once and no fragments will follow.
        if input.as_object().is_some_and(|object| !object.is_empty()) {
            entry.arguments = input.to_string();
        }
    }

    fn push_arguments(&mut self, index: u64, fragment: &str) {
        self.pending
            .entry(index)
            .or_default()
            .arguments
            .push_str(fragment);
    }

    /// Emits in index order: the model chose that order, and for sequential
    /// edits to the same file it matters.
    fn finish(self) -> Vec<Value> {
        let mut assembled: Vec<(u64, PartialToolCall)> = self.pending.into_iter().collect();
        assembled.sort_by_key(|(index, _)| *index);
        assembled
            .into_iter()
            .filter(|(_, call)| !call.name.is_empty())
            .map(|(_, call)| call.into_value())
            .collect()
    }
}

#[derive(Default)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

impl PartialToolCall {
    fn into_value(self) -> Value {
        let id = if self.id.is_empty() {
            format!("call_{}", &uuid::Uuid::new_v4().simple().to_string()[..8])
        } else {
            self.id
        };
        let arguments = if self.arguments.trim().is_empty() {
            "{}".to_string()
        } else {
            self.arguments
        };
        json!({
            "id": id,
            "type": "function",
            "function": { "name": self.name, "arguments": arguments },
        })
    }
}

/// Read an Anthropic SSE response into text deltas plus assembled tool calls.
///
/// The shape differs from OpenAI enough to need its own reader: a tool call is
/// announced by `content_block_start` carrying its id and name, and only then
/// are the arguments streamed as `input_json_delta` fragments under the same
/// block index.
fn stream_anthropic_events_from_response(
    response: reqwest::Response,
) -> impl Stream<Item = anyhow::Result<ChatStreamEvent>> + Send {
    try_stream! {
        let mut chunks = response.bytes_stream();
        let mut buffer = String::new();
        let mut assembler = ToolCallAssembler::default();

        'outer: while let Some(chunk) = chunks.next().await {
            buffer.push_str(&String::from_utf8_lossy(&chunk?));

            while let Some((position, delimiter)) = sse_frame_boundary(&buffer) {
                let frame: String = buffer.drain(..position + delimiter).collect();

                for line in frame.lines() {
                    let Some(data) = line.trim().strip_prefix("data:") else {
                        continue;
                    };
                    let Ok(value) = serde_json::from_str::<Value>(data.trim()) else {
                        continue;
                    };

                    match value["type"].as_str().unwrap_or_default() {
                        "content_block_start"
                            if value["content_block"]["type"] == "tool_use" =>
                        {
                            assembler.start_block(
                                value["index"].as_u64().unwrap_or(0),
                                value["content_block"]["id"].as_str().unwrap_or_default(),
                                value["content_block"]["name"].as_str().unwrap_or_default(),
                                &value["content_block"]["input"],
                            );
                        }
                        "content_block_delta" => {
                            let delta = &value["delta"];
                            match delta["type"].as_str().unwrap_or_default() {
                                "text_delta" => {
                                    if let Some(text) = delta["text"].as_str() {
                                        if !text.is_empty() {
                                            yield ChatStreamEvent::Text(text.to_string());
                                        }
                                    }
                                }
                                "thinking_delta" => {
                                    if let Some(text) = delta["thinking"].as_str() {
                                        if !text.is_empty() {
                                            yield ChatStreamEvent::Reasoning(text.to_string());
                                        }
                                    }
                                }
                                "input_json_delta" => {
                                    if let Some(fragment) = delta["partial_json"].as_str() {
                                        assembler.push_arguments(
                                            value["index"].as_u64().unwrap_or(0),
                                            fragment,
                                        );
                                    }
                                }
                                _ => {}
                            }
                        }
                        // Surfaced rather than swallowed: a mid-stream refusal
                        // or overload otherwise looks like an empty answer.
                        "error" => {
                            let message = value["error"]["message"]
                                .as_str()
                                .unwrap_or("unknown Anthropic stream error");
                            Err(anyhow!("Anthropic stream error: {message}"))?;
                        }
                        "message_stop" => break 'outer,
                        _ => {}
                    }
                }
            }
        }

        let calls = assembler.finish();
        if !calls.is_empty() {
            yield ChatStreamEvent::ToolCalls(calls);
        }
    }
}

fn sse_frame_boundary(buffer: &str) -> Option<(usize, usize)> {
    match (buffer.find("\n\n"), buffer.find("\r\n\r\n")) {
        (Some(lf), Some(crlf)) if crlf < lf => Some((crlf, 4)),
        (Some(lf), _) => Some((lf, 2)),
        (None, Some(crlf)) => Some((crlf, 4)),
        (None, None) => None,
    }
}

fn stream_tokens_from_response(
    response: reqwest::Response,
) -> impl Stream<Item = anyhow::Result<String>> + Send {
    try_stream! {
        let mut chunks = response.bytes_stream();
        let mut buffer = String::new();
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(pos) = buffer.find('\n') {
                let mut line = buffer[..pos].to_string();
                buffer.drain(..=pos);
                if line.ends_with('\r') {
                    line.pop();
                }
                for token in stream_tokens_from_line(&line)? {
                    yield token;
                }
            }
        }
        if !buffer.trim().is_empty() {
            for token in stream_tokens_from_line(&buffer)? {
                yield token;
            }
        }
    }
}

fn stream_tokens_from_line(line: &str) -> anyhow::Result<Vec<String>> {
    let line = line.trim();
    if line.is_empty() || line.starts_with(':') {
        return Ok(Vec::new());
    }
    let data = line.strip_prefix("data:").map(str::trim).unwrap_or(line);
    if data.is_empty() || data == "[DONE]" {
        return Ok(Vec::new());
    }
    let payload: Value = serde_json::from_str(data).with_context(|| {
        format!(
            "failed to parse streaming chunk: {}",
            data.chars().take(200).collect::<String>()
        )
    })?;
    Ok(extract_stream_tokens(&payload))
}

pub fn candidate_chat_urls(cfg: &AppConfig) -> Vec<String> {
    let base = cfg.llama_base_url.trim_end_matches('/');
    if base.is_empty() {
        return Vec::new();
    }
    if base.ends_with("/chat/completions") || base.ends_with("/completion") {
        return vec![base.to_string()];
    }
    if let Some(root) = base.strip_suffix("/v1") {
        return vec![
            format!("{base}/chat/completions"),
            format!("{root}/completion"),
        ];
    }
    vec![
        format!("{base}/chat/completions"),
        format!("{base}/v1/chat/completions"),
        format!("{base}/completion"),
    ]
}

pub fn candidate_model_urls(cfg: &AppConfig) -> Vec<String> {
    let base = cfg.llama_base_url.trim_end_matches('/');
    if base.is_empty() {
        return Vec::new();
    }
    let root = base
        .strip_suffix("/chat/completions")
        .or_else(|| base.strip_suffix("/completion"))
        .unwrap_or(base);
    if root.ends_with("/v1") {
        vec![format!("{root}/models")]
    } else {
        vec![format!("{root}/models"), format!("{root}/v1/models")]
    }
}

fn headers(cfg: &AppConfig) -> anyhow::Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if !cfg.llama_api_key.trim().is_empty() {
        let value = format!("Bearer {}", cfg.llama_api_key.trim());
        headers.insert(AUTHORIZATION, HeaderValue::from_str(&value)?);
    }
    Ok(headers)
}

fn anthropic_headers(cfg: &AppConfig) -> anyhow::Result<HeaderMap> {
    if cfg.llama_api_key.trim().is_empty() {
        bail!("Anthropic API key is missing");
    }
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
        "x-api-key",
        HeaderValue::from_str(cfg.llama_api_key.trim())?,
    );
    headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
    Ok(headers)
}

fn anthropic_messages_url(cfg: &AppConfig) -> String {
    let base = cfg.llama_base_url.trim().trim_end_matches('/');
    if base.ends_with("/messages") {
        base.to_string()
    } else if base.ends_with("/v1") {
        format!("{base}/messages")
    } else {
        format!("{base}/v1/messages")
    }
}

fn anthropic_messages(messages: &[Value]) -> (String, Vec<Value>) {
    let system = messages
        .iter()
        .filter(|message| message.get("role").and_then(Value::as_str) == Some("system"))
        .filter_map(|message| message.get("content"))
        .map(extract_message_text)
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");

    let mut converted = Vec::<Value>::new();
    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        let (role, mut blocks) = match role {
            "system" => continue,
            "assistant" | "gnome" => {
                let mut blocks =
                    anthropic_content_blocks(message.get("content").unwrap_or(&Value::Null));
                if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
                    for call in calls {
                        let function = call.get("function").unwrap_or(&Value::Null);
                        let name = function.get("name").and_then(Value::as_str).unwrap_or("");
                        if name.is_empty() {
                            continue;
                        }
                        let input = function
                            .get("arguments")
                            .and_then(Value::as_str)
                            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                            .unwrap_or_else(|| json!({}));
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": call.get("id").and_then(Value::as_str).unwrap_or("call"),
                            "name": name,
                            "input": input,
                        }));
                    }
                }
                ("assistant", blocks)
            }
            "tool" => (
                "user",
                vec![json!({
                    "type": "tool_result",
                    "tool_use_id": message
                        .get("tool_call_id")
                        .and_then(Value::as_str)
                        .unwrap_or("call"),
                    "content": extract_message_text(
                        message.get("content").unwrap_or(&Value::Null)
                    ),
                })],
            ),
            _ => (
                "user",
                anthropic_content_blocks(message.get("content").unwrap_or(&Value::Null)),
            ),
        };
        if blocks.is_empty() {
            blocks.push(json!({"type": "text", "text": ""}));
        }

        if let Some(previous) = converted.last_mut()
            && previous.get("role").and_then(Value::as_str) == Some(role)
            && let Some(content) = previous.get_mut("content").and_then(Value::as_array_mut)
        {
            content.extend(blocks);
        } else {
            converted.push(json!({"role": role, "content": blocks}));
        }
    }
    (system, converted)
}

fn anthropic_content_blocks(content: &Value) -> Vec<Value> {
    match content {
        Value::Array(items) => items
            .iter()
            .filter_map(|item| {
                let kind = item.get("type").and_then(Value::as_str);
                if kind == Some("text") {
                    return Some(json!({
                        "type": "text",
                        "text": item.get("text").and_then(Value::as_str).unwrap_or(""),
                    }));
                }
                if matches!(kind, Some("image_url" | "input_image" | "image")) {
                    let url = item
                        .pointer("/image_url/url")
                        .or_else(|| item.get("image_url"))
                        .or_else(|| item.get("url"))
                        .and_then(Value::as_str)?;
                    if let Some(data) = url.strip_prefix("data:")
                        && let Some((media_type, encoded)) = data.split_once(";base64,")
                    {
                        return Some(json!({
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": media_type,
                                "data": encoded,
                            }
                        }));
                    }
                    return Some(json!({
                        "type": "image",
                        "source": {"type": "url", "url": url},
                    }));
                }
                let text = extract_message_text(item);
                (!text.is_empty()).then(|| json!({"type": "text", "text": text}))
            })
            .collect(),
        _ => {
            let text = extract_message_text(content);
            vec![json!({"type": "text", "text": text})]
        }
    }
}

fn anthropic_tool_schema(tool: &Value) -> Option<Value> {
    let function = tool.get("function")?;
    let name = function.get("name").and_then(Value::as_str)?;
    Some(json!({
        "name": name,
        "description": function
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or(""),
        "input_schema": function
            .get("parameters")
            .cloned()
            .unwrap_or_else(|| json!({"type": "object"})),
    }))
}

fn anthropic_response(payload: &Value) -> LlamaResponse {
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    if let Some(blocks) = payload.get("content").and_then(Value::as_array) {
        for block in blocks {
            match block.get("type").and_then(Value::as_str) {
                Some("text") => {
                    text.push_str(block.get("text").and_then(Value::as_str).unwrap_or(""));
                }
                Some("tool_use") => {
                    let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                    if !name.is_empty() {
                        tool_calls.push(json!({
                            "id": block.get("id").and_then(Value::as_str).unwrap_or("call"),
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": block
                                    .get("input")
                                    .cloned()
                                    .unwrap_or_else(|| json!({}))
                                    .to_string(),
                            }
                        }));
                    }
                }
                _ => {}
            }
        }
    }
    LlamaResponse {
        content: text,
        tool_calls,
    }
}

fn stream_anthropic_tokens(
    response: reqwest::Response,
) -> impl Stream<Item = anyhow::Result<String>> + Send {
    try_stream! {
        let mut chunks = response.bytes_stream();
        let mut buffer = String::new();
        while let Some(chunk) = chunks.next().await {
            buffer.push_str(&String::from_utf8_lossy(&chunk?));
            while let Some(pos) = buffer.find('\n') {
                let mut line = buffer[..pos].to_string();
                buffer.drain(..=pos);
                if line.ends_with('\r') {
                    line.pop();
                }
                let line = line.trim();
                let Some(data) = line.strip_prefix("data:").map(str::trim) else {
                    continue;
                };
                if data.is_empty() || data == "[DONE]" {
                    continue;
                }
                let payload: Value = serde_json::from_str(data)?;
                if payload.get("type").and_then(Value::as_str) == Some("error") {
                    let message = payload
                        .pointer("/error/message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown Anthropic stream error");
                    Err(anyhow!("Anthropic stream error: {message}"))?;
                }
                if payload.pointer("/delta/type").and_then(Value::as_str)
                    == Some("text_delta")
                    && let Some(text) = payload.pointer("/delta/text").and_then(Value::as_str)
                    && !text.is_empty()
                {
                    yield text.to_string();
                }
            }
        }
    }
}

pub fn parse_models(payload: &Value) -> Vec<ModelInfo> {
    let mut out = Vec::new();
    if let Some(models) = payload.get("models").and_then(Value::as_array) {
        for item in models {
            if let Some(model) = parse_model_item(item) {
                out.push(model);
            }
        }
    }
    if !out.is_empty() {
        return out;
    }
    if let Some(data) = payload.get("data").and_then(Value::as_array) {
        for item in data {
            if let Some(model) = parse_model_item(item) {
                out.push(model);
            }
        }
    }
    out
}

fn parse_model_item(item: &Value) -> Option<ModelInfo> {
    if let Some(text) = item.as_str() {
        return Some(ModelInfo {
            id: text.trim().to_string(),
            capabilities: Vec::new(),
        });
    }
    let obj = item.as_object()?;
    let id = obj
        .get("id")
        .or_else(|| obj.get("name"))
        .or_else(|| obj.get("model"))
        .and_then(Value::as_str)?
        .trim()
        .to_string();
    if id.is_empty() {
        return None;
    }
    let mut capabilities = obj
        .get("capabilities")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(|item| item.to_lowercase())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    for path in [
        "/architecture/input_modalities",
        "/input_modalities",
        "/modalities",
    ] {
        let Some(modalities) = item.pointer(path).and_then(Value::as_array) else {
            continue;
        };
        for modality in modalities.iter().filter_map(Value::as_str) {
            let modality = modality.to_lowercase();
            if !capabilities.contains(&modality) {
                capabilities.push(modality);
            }
        }
    }
    Some(ModelInfo { id, capabilities })
}

pub fn extract_message_text(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(|item| {
                if let Some(text) = item.as_str() {
                    return text.to_string();
                }
                if let Some(obj) = item.as_object() {
                    if obj.get("type").and_then(Value::as_str) == Some("text") {
                        return obj
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                    }
                    if let Some(text) = obj.get("text").and_then(Value::as_str) {
                        return text.to_string();
                    }
                    if let Some(content) = obj.get("content") {
                        return extract_message_text(content);
                    }
                }
                String::new()
            })
            .collect::<String>(),
        Value::Object(obj) => obj
            .get("content")
            .map(extract_message_text)
            .or_else(|| obj.get("text").map(extract_message_text))
            .unwrap_or_else(|| value.to_string()),
        other => other.to_string(),
    }
}

pub fn prompt_from_messages(messages: &[Value]) -> String {
    let mut lines = Vec::new();
    for msg in messages {
        let role = msg.get("role").and_then(Value::as_str).unwrap_or("user");
        let content = msg
            .get("content")
            .map(extract_message_text)
            .unwrap_or_default();
        if content.trim().is_empty() {
            continue;
        }
        let label = match role {
            "system" => "System",
            "assistant" | "gnome" => "Assistant",
            _ => "User",
        };
        lines.push(format!("{label}: {content}"));
    }
    lines.push("Assistant:".into());
    lines.join("\n\n")
}

pub fn extract_response_text(payload: &Value) -> String {
    if let Some(first) = payload
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
    {
        if let Some(message) = first.get("message").and_then(|m| m.get("content")) {
            return extract_message_text(message);
        }
        if let Some(delta) = first.get("delta").and_then(|m| m.get("content")) {
            return extract_message_text(delta);
        }
        if let Some(text) = first.get("text") {
            return extract_message_text(text);
        }
    }
    if let Some(message) = payload.get("message").and_then(|m| m.get("content")) {
        return extract_message_text(message);
    }
    payload
        .get("content")
        .map(extract_message_text)
        .unwrap_or_default()
}

pub fn extract_stream_tokens(payload: &Value) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(choices) = payload.get("choices").and_then(Value::as_array) {
        for choice in choices {
            if let Some(content) = choice.get("delta").and_then(|delta| delta.get("content")) {
                let token = extract_message_text(content);
                if !token.is_empty() {
                    out.push(token);
                }
            }
            if let Some(content) = choice
                .get("message")
                .and_then(|message| message.get("content"))
            {
                let token = extract_message_text(content);
                if !token.is_empty() {
                    out.push(token);
                }
            }
            if let Some(text) = choice.get("text") {
                let token = extract_message_text(text);
                if !token.is_empty() {
                    out.push(token);
                }
            }
        }
        if !out.is_empty() {
            return out;
        }
    }
    for key in ["content", "response", "text"] {
        if let Some(value) = payload.get(key) {
            let token = extract_message_text(value);
            if !token.is_empty() {
                out.push(token);
            }
        }
    }
    if let Some(content) = payload
        .get("message")
        .and_then(|message| message.get("content"))
    {
        let token = extract_message_text(content);
        if !token.is_empty() {
            out.push(token);
        }
    }
    out
}

pub fn extract_tool_calls(payload: &Value) -> Vec<Value> {
    if let Some(first) = payload
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
    {
        if let Some(calls) = first
            .get("message")
            .and_then(|message| message.get("tool_calls"))
            .and_then(Value::as_array)
        {
            return calls.clone();
        }
        if let Some(calls) = first
            .get("delta")
            .and_then(|message| message.get("tool_calls"))
            .and_then(Value::as_array)
        {
            return calls.clone();
        }
    }
    payload
        .get("message")
        .and_then(|message| message.get("tool_calls"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_openai_streaming_content_delta() {
        let line = r#"data: {"choices":[{"delta":{"content":"hel"}}]}"#;
        let tokens = stream_tokens_from_line(line).unwrap();
        assert_eq!(tokens, vec!["hel"]);
    }

    #[test]
    fn parses_llama_completion_streaming_content() {
        let line = r#"data: {"content":"lo"}"#;
        let tokens = stream_tokens_from_line(line).unwrap();
        assert_eq!(tokens, vec!["lo"]);
    }

    #[test]
    fn assembles_tool_call_arguments_split_across_fragments() {
        let mut assembler = ToolCallAssembler::default();
        for fragment in [
            json!({"tool_calls":[{"index":0,"id":"call_a","function":{"name":"Bash"}}]}),
            json!({"tool_calls":[{"index":0,"function":{"arguments":"{\"comm"}}]}),
            json!({"tool_calls":[{"index":0,"function":{"arguments":"and\":\"ls\"}"}}]}),
        ] {
            assembler.absorb(&fragment);
        }

        let calls = assembler.finish();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["id"], "call_a");
        assert_eq!(calls[0]["function"]["name"], "Bash");
        // The whole argument object, not just the last fragment.
        assert_eq!(calls[0]["function"]["arguments"], r#"{"command":"ls"}"#);
    }

    #[test]
    fn keeps_parallel_tool_calls_in_index_order() {
        let mut assembler = ToolCallAssembler::default();
        // Deliberately out of order on the wire.
        assembler.absorb(
            &json!({"tool_calls":[{"index":1,"id":"b","function":{"name":"Write","arguments":"{}"}}]}),
        );
        assembler.absorb(
            &json!({"tool_calls":[{"index":0,"id":"a","function":{"name":"Read","arguments":"{}"}}]}),
        );

        let calls = assembler.finish();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["function"]["name"], "Read");
        assert_eq!(calls[1]["function"]["name"], "Write");
    }

    #[test]
    fn drops_nameless_fragments_and_defaults_empty_arguments() {
        let mut assembler = ToolCallAssembler::default();
        // Index 1 never receives a name: a provider hiccup, not a call.
        assembler.absorb(&json!({"tool_calls":[{"index":1,"function":{"arguments":"{}"}}]}));
        assembler.absorb(&json!({"tool_calls":[{"index":0,"function":{"name":"TaskList"}}]}));

        let calls = assembler.finish();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["function"]["name"], "TaskList");
        assert_eq!(calls[0]["function"]["arguments"], "{}");
        assert!(
            calls[0]["id"].as_str().is_some_and(|id| !id.is_empty()),
            "a call with no upstream id still needs one for the tool result to match"
        );
    }

    #[test]
    fn assembles_anthropic_tool_calls_from_block_start_and_json_fragments() {
        let mut assembler = ToolCallAssembler::default();
        // Anthropic names the call up front, then streams its arguments.
        assembler.start_block(0, "toolu_1", "Bash", &json!({}));
        assembler.push_arguments(0, "{\"comm");
        assembler.push_arguments(0, "and\":\"ls\"}");

        let calls = assembler.finish();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["id"], "toolu_1");
        assert_eq!(calls[0]["function"]["name"], "Bash");
        assert_eq!(calls[0]["function"]["arguments"], r#"{"command":"ls"}"#);
    }

    #[test]
    fn anthropic_input_delivered_whole_is_not_duplicated() {
        // When `content_block_start` already carries the arguments, no
        // `input_json_delta` follows; concatenating both would produce
        // unparseable JSON.
        let mut assembler = ToolCallAssembler::default();
        assembler.start_block(0, "toolu_2", "Read", &json!({"path": "src/main.rs"}));

        let calls = assembler.finish();
        assert_eq!(calls.len(), 1);
        let arguments = calls[0]["function"]["arguments"].as_str().unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(arguments).unwrap()["path"],
            "src/main.rs"
        );
    }

    #[test]
    fn anthropic_parallel_tool_blocks_keep_their_own_arguments() {
        let mut assembler = ToolCallAssembler::default();
        assembler.start_block(0, "toolu_a", "Read", &json!({}));
        assembler.start_block(1, "toolu_b", "Grep", &json!({}));
        // Fragments interleave across block indices on the wire.
        assembler.push_arguments(0, "{\"path\":");
        assembler.push_arguments(1, "{\"pattern\":");
        assembler.push_arguments(0, "\"a.rs\"}");
        assembler.push_arguments(1, "\"fn main\"}");

        let calls = assembler.finish();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["function"]["arguments"], r#"{"path":"a.rs"}"#);
        assert_eq!(
            calls[1]["function"]["arguments"],
            r#"{"pattern":"fn main"}"#
        );
    }

    /// Drives the real reader over a real socket, so frame splitting and event
    /// dispatch are covered and not just the assembler underneath them.
    #[tokio::test]
    async fn anthropic_stream_yields_text_then_assembled_tool_calls() {
        use axum::{Router, routing::post};

        // A faithful slice of the wire format, deliberately split so one tool
        // call's arguments straddle two SSE frames.
        const BODY: &str = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":9}}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Ma uit.\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"hmm\"}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_9\",\"name\":\"Grep\",\"input\":{}}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"pattern\\\":\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"fn main\\\"}\"}}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route("/v1/messages", post(|| async { BODY }));
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let mut cfg = AppConfig::default();
        cfg.provider_protocol = "anthropic".into();
        cfg.llama_base_url = format!("http://{addr}");
        cfg.llama_api_key = "test-key".into();

        let mut stream = LlamaClient::new()
            .chat_stream_with_tools(
                &cfg,
                "claude-sonnet-5",
                vec![json!({"role": "user", "content": "cauta"})],
                0.3,
                vec![json!({"type": "function", "function": {
                    "name": "Grep",
                    "description": "search",
                    "parameters": {"type": "object", "properties": {}},
                }})],
                None,
            )
            .await
            .unwrap();

        let mut text = String::new();
        let mut reasoning = String::new();
        let mut calls = Vec::new();
        while let Some(event) = stream.next().await {
            match event.unwrap() {
                ChatStreamEvent::Text(chunk) => text.push_str(&chunk),
                ChatStreamEvent::Reasoning(chunk) => reasoning.push_str(&chunk),
                ChatStreamEvent::ToolCalls(batch) => calls = batch,
            }
        }

        assert_eq!(text, "Ma uit.");
        assert_eq!(reasoning, "hmm");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["id"], "toolu_9");
        assert_eq!(calls[0]["function"]["name"], "Grep");
        assert_eq!(
            calls[0]["function"]["arguments"], r#"{"pattern":"fn main"}"#,
            "arguments split across frames must be rejoined"
        );
        server.abort();
    }

    #[tokio::test]
    async fn anthropic_stream_surfaces_a_mid_stream_error() {
        use axum::{Router, routing::post};

        const BODY: &str = concat!(
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
            "data: {\"type\":\"error\",\"error\":{\"message\":\"overloaded_error\"}}\n\n",
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route("/v1/messages", post(|| async { BODY }));
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let mut cfg = AppConfig::default();
        cfg.provider_protocol = "anthropic".into();
        cfg.llama_base_url = format!("http://{addr}");
        cfg.llama_api_key = "test-key".into();

        let mut stream = LlamaClient::new()
            .chat_stream_with_tools(
                &cfg,
                "claude-sonnet-5",
                vec![json!({"role": "user", "content": "hi"})],
                0.3,
                Vec::new(),
                None,
            )
            .await
            .unwrap();

        let mut failed = false;
        while let Some(event) = stream.next().await {
            if let Err(error) = event {
                // An overload must not read as an empty answer.
                assert!(error.to_string().contains("overloaded_error"), "{error}");
                failed = true;
            }
        }
        assert!(failed, "the stream error must reach the caller");
        server.abort();
    }

    #[test]
    fn splits_sse_frames_on_both_line_endings() {
        assert_eq!(sse_frame_boundary("data: a\n\ndata: b"), Some((7, 2)));
        assert_eq!(sse_frame_boundary("data: a\r\n\r\ndata: b"), Some((7, 4)));
        // A partial frame stays in the buffer.
        assert_eq!(sse_frame_boundary("data: a\n"), None);
    }

    #[test]
    fn parses_openrouter_input_modalities_as_capabilities() {
        let models = parse_models(&json!({
            "data": [{
                "id": "nvidia/nemotron-3-nano-omni-30b-a3b-reasoning:free",
                "architecture": {
                    "input_modalities": ["text", "image"],
                    "output_modalities": ["text"]
                }
            }]
        }));

        assert_eq!(models.len(), 1);
        assert!(models[0].capabilities.iter().any(|item| item == "image"));
    }

    #[test]
    fn model_ids_are_sorted_deduplicated_and_keep_the_active_model_first() {
        let ids = model_ids(
            vec![model("zeta"), model("alpha"), model("zeta"), model("  ")],
            "current",
        );
        assert_eq!(ids, vec!["current", "alpha", "zeta"]);

        let ids = model_ids(vec![model("zeta"), model("alpha")], "zeta");
        assert_eq!(ids, vec!["zeta", "alpha"]);
    }

    #[test]
    fn account_provider_fallbacks_never_invent_codex_model_ids() {
        let openai = known_models("openai-account")
            .into_iter()
            .map(|model| model.id)
            .collect::<Vec<_>>();
        assert_eq!(openai, vec!["default"]);

        let anthropic = known_models("anthropic-account")
            .into_iter()
            .map(|model| model.id)
            .collect::<Vec<_>>();
        assert_eq!(anthropic, vec!["default", "sonnet", "opus", "haiku"]);
    }

    #[test]
    fn codex_account_models_reject_a_stale_active_model() {
        let models = vec![model("gpt-account-a"), model("gpt-account-b")];
        assert_eq!(
            codex_account_model_ids(models.clone(), "not-in-model-list"),
            vec!["default", "gpt-account-a", "gpt-account-b"]
        );
        assert_eq!(
            codex_account_model_ids(models, "gpt-account-b"),
            vec!["gpt-account-b", "default", "gpt-account-a"]
        );
    }
}

pub fn messages_have_images(messages: &[Value]) -> bool {
    messages.iter().any(|msg| {
        msg.get("images").is_some_and(|images| !images.is_null())
            || content_has_image(msg.get("content").unwrap_or(&Value::Null))
    })
}

fn content_has_image(value: &Value) -> bool {
    match value {
        Value::Array(items) => items.iter().any(content_has_image),
        Value::Object(obj) => {
            matches!(
                obj.get("type").and_then(Value::as_str),
                Some("image_url" | "input_image" | "image")
            ) || obj.contains_key("image_url")
                || obj.contains_key("image")
        }
        _ => false,
    }
}
