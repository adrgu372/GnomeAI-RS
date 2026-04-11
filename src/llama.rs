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

    pub async fn list_models(&self, cfg: &AppConfig) -> anyhow::Result<Vec<ModelInfo>> {
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
                json!({
                    "model": model,
                    "messages": messages,
                    "temperature": temperature,
                    "max_tokens": cfg.llama_max_tokens,
                    "stream": true,
                })
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
                    "max_tokens": cfg.llama_max_tokens,
                    "stream": false,
                });
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
