use std::pin::Pin;

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

pub type TokenStream = Pin<Box<dyn Stream<Item = anyhow::Result<String>> + Send>>;

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
        if cfg.provider_protocol == "anthropic" {
            return Ok(Vec::new());
        }
        ensure_web_provider_supported(cfg)?;
        let mut last_error = None;
        for url in candidate_model_urls(cfg) {
            let result = self.http.get(&url).headers(headers(cfg)?).send().await;
            let response = match result {
                Ok(response) => response,
                Err(err) => {
                    last_error = Some(err.into());
                    continue;
                }
            };
            if response.status().as_u16() == 404 {
                continue;
            }
            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                last_error = Some(anyhow!("{url} returned {status}: {body}"));
                continue;
            }
            let payload: Value = response.json().await?;
            let models = parse_models(&payload);
            if !models.is_empty() {
                return Ok(models);
            }
        }
        if let Some(err) = last_error {
            Err(err)
        } else {
            Ok(Vec::new())
        }
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

            let payload = if (is_completion || cfg.llama_api_mode == "completion") && !has_images {
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

            let payload = if (is_completion || cfg.llama_api_mode == "completion")
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
}

fn ensure_web_provider_supported(cfg: &AppConfig) -> anyhow::Result<()> {
    match cfg.provider_protocol.as_str() {
        "codex" => bail!(
            "OpenAI account login is available in the terminal agent; choose an API provider in WebTool"
        ),
        "claude-cli" => bail!(
            "Claude Code account login is available in the terminal agent; choose an API provider in WebTool"
        ),
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
    let capabilities = obj
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
