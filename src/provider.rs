//! Coding-agent provider abstraction.
//!
//! One trait, two realities: a hosted OpenAI-compatible endpoint and a local
//! `llama-server`. They speak nominally the same wire format and differ in
//! every detail that matters, so the normalisation lives here and the agent
//! loop never learns about it.
//!
//! The two things that actually break integrations:
//!
//!   1. Tool-call arguments arrive as fragments spread across many SSE chunks,
//!      keyed by `index`. The `id` and `name` usually appear only in the first
//!      fragment. Accumulate by index or you will lose half of every call.
//!
//!   2. Local models often emit tool calls as JSON *inside the text content*
//!      instead of the structured field, because the chat template did not
//!      wire up tool support. `salvage_tool_calls` catches that case. Without
//!      it, a self-hosted setup looks broken while the model is doing its best.

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use futures_util::stream::{BoxStream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::AsyncWriteExt;

use crate::sandbox::SandboxMode;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema for the arguments.
    pub parameters: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Raw JSON. Parsed by the dispatcher, not here — a malformed argument is
    /// a message for the model, not a transport error.
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        content: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        call_id: String,
        content: String,
    },
}

/// Account-backed providers own their execution loop and do not receive the
/// GnomeAI tool registry. Forward only explicit host context that they still
/// need (skills and shared memory), while keeping unrelated internal system
/// messages out of the delegated user prompt.
pub(crate) fn delegated_host_context(messages: &[Message]) -> String {
    const MARKERS: &[&str] = &[
        "<activated_skill ",
        "--- Installed Agent Skills catalog ---",
        "Cross-Conversation Memory",
    ];
    let mut blocks = Vec::new();
    for message in messages {
        let Message::System { content } = message else {
            continue;
        };
        let start = MARKERS
            .iter()
            .filter_map(|marker| content.find(marker))
            .min();
        if let Some(start) = start {
            blocks.push(content[start..].trim().to_string());
        }
    }
    blocks.join("\n\n")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Stop,
    ToolCalls,
    Length,
    Cancelled,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Usage {
    pub input_tokens: i64,
    pub output_tokens: i64,
}

#[derive(Debug, Clone)]
pub enum Delta {
    Text(String),
    Reasoning(String),
    /// Emitted once the call is complete, not per fragment. Fragment assembly
    /// is this module's problem, not the loop's.
    ToolCall(ToolCall),
    Done {
        reason: StopReason,
        usage: Usage,
    },
}

pub struct Request {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub max_tokens: u32,
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    /// Account-backed coding CLIs execute their own tools instead of returning
    /// tool calls to the GnomeAI registry. In `normal` mode the outer agent
    /// therefore asks once before delegating the turn with full OS access.
    fn delegates_execution(&self) -> bool {
        false
    }
    async fn stream(&self, req: Request) -> Result<BoxStream<'static, Result<Delta>>>;
}

// ---------------------------------------------------------------------------
// OpenAI-compatible (covers llama-server, vLLM, OpenRouter, and the rest)
// ---------------------------------------------------------------------------

pub struct OpenAiCompatible {
    client: reqwest::Client,
    name: String,
    base_url: String,
    api_key: Option<String>,
    /// llama-server rejects `tool_choice` on some builds. Off by default.
    pub send_tool_choice: bool,
    /// Current OpenAI reasoning models use `max_completion_tokens`; most
    /// compatibility providers still implement `max_tokens`.
    pub use_max_completion_tokens: bool,
}

impl OpenAiCompatible {
    pub fn named(
        name: impl Into<String>,
        base_url: impl Into<String>,
        api_key: Option<String>,
    ) -> Self {
        Self {
            client: reqwest::Client::builder()
                // No global timeout: a streaming response is long-lived by
                // design. Bound the idle gap instead, in the loop.
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("client"),
            name: name.into(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key,
            send_tool_choice: false,
            use_max_completion_tokens: false,
        }
    }

    fn body(&self, req: &Request) -> Value {
        let messages: Vec<Value> = req.messages.iter().map(encode_message).collect();

        let mut body = json!({
            "model": req.model,
            "messages": messages,
            "stream": true,
            "stream_options": { "include_usage": true },
        });
        if self.use_max_completion_tokens {
            body["max_completion_tokens"] = json!(req.max_tokens);
        } else {
            body["max_tokens"] = json!(req.max_tokens);
        }

        if !req.tools.is_empty() {
            body["tools"] = json!(
                req.tools
                    .iter()
                    .map(|t| json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters,
                        }
                    }))
                    .collect::<Vec<_>>()
            );

            if self.send_tool_choice {
                body["tool_choice"] = json!("auto");
            }
        }

        body
    }
}

fn encode_message(m: &Message) -> Value {
    match m {
        Message::System { content } => json!({ "role": "system", "content": content }),
        Message::User { content } => json!({ "role": "user", "content": content }),
        Message::Assistant {
            content,
            tool_calls,
        } => {
            let mut v = json!({ "role": "assistant", "content": content });
            if !tool_calls.is_empty() {
                v["tool_calls"] = json!(
                    tool_calls
                        .iter()
                        .map(|c| json!({
                            "id": c.id,
                            "type": "function",
                            "function": { "name": c.name, "arguments": c.arguments }
                        }))
                        .collect::<Vec<_>>()
                );
            }
            v
        }
        Message::Tool { call_id, content } => json!({
            "role": "tool",
            "tool_call_id": call_id,
            "content": content,
        }),
    }
}

#[async_trait]
impl Provider for OpenAiCompatible {
    fn name(&self) -> &str {
        &self.name
    }

    async fn stream(&self, req: Request) -> Result<BoxStream<'static, Result<Delta>>> {
        let url = format!("{}/chat/completions", self.base_url);
        let mut body = self.body(&req);
        let mut builder = self.client.post(&url).json(&body);
        if let Some(key) = &self.api_key {
            builder = builder.bearer_auth(key);
        }

        let mut resp = builder.send().await.context("request failed")?;
        // A few otherwise-compatible servers reject the optional usage
        // extension. Retry once without it instead of making every preset
        // carry a brittle compatibility flag.
        if resp.status() == reqwest::StatusCode::BAD_REQUEST {
            let status = resp.status();
            let error_body = resp.text().await.unwrap_or_default();
            if error_body.contains("stream_options") {
                body.as_object_mut()
                    .map(|object| object.remove("stream_options"));
                let mut retry = self.client.post(&url).json(&body);
                if let Some(key) = &self.api_key {
                    retry = retry.bearer_auth(key);
                }
                resp = retry.send().await.context("provider retry failed")?;
            } else {
                bail!(
                    "provider returned {status}: {}",
                    error_body.chars().take(500).collect::<String>()
                );
            }
        }
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!(
                "provider returned {status}: {}",
                body.chars().take(500).collect::<String>()
            );
        }

        Ok(Box::pin(parse_sse(resp)))
    }
}

// ---------------------------------------------------------------------------
// Anthropic Messages API
// ---------------------------------------------------------------------------

pub struct Anthropic {
    client: reqwest::Client,
    name: String,
    base_url: String,
    api_key: String,
}

impl Anthropic {
    pub fn new(
        name: impl Into<String>,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            client: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("client"),
            name: name.into(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
        }
    }

    fn body(&self, req: &Request) -> Value {
        let system = req
            .messages
            .iter()
            .filter_map(|message| match message {
                Message::System { content } => Some(content.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        let messages = encode_anthropic_messages(&req.messages);
        let mut body = json!({
            "model": req.model,
            "messages": messages,
            "max_tokens": req.max_tokens,
            "stream": true,
        });
        if !system.is_empty() {
            body["system"] = json!(system);
        }
        // Gnome's provider-neutral transcript stores text and tool calls, but
        // not Anthropic's signed thinking blocks. Disable adaptive thinking on
        // models that enable it by default so multi-round tool conversations
        // remain valid instead of dropping required signed state.
        if req.model.starts_with("claude-sonnet-5")
            || req.model.starts_with("claude-opus-4-7")
            || req.model.starts_with("claude-opus-4-8")
        {
            body["thinking"] = json!({ "type": "disabled" });
        }
        if !req.tools.is_empty() {
            body["tools"] = json!(
                req.tools
                    .iter()
                    .map(|tool| json!({
                        "name": tool.name,
                        "description": tool.description,
                        "input_schema": tool.parameters,
                    }))
                    .collect::<Vec<_>>()
            );
        }
        body
    }
}

#[async_trait]
impl Provider for Anthropic {
    fn name(&self) -> &str {
        &self.name
    }

    async fn stream(&self, req: Request) -> Result<BoxStream<'static, Result<Delta>>> {
        let url = format!("{}/messages", self.base_url);
        let resp = self
            .client
            .post(url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&self.body(&req))
            .send()
            .await
            .context("Anthropic request failed")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!(
                "Anthropic returned {status}: {}",
                body.chars().take(500).collect::<String>()
            );
        }
        Ok(Box::pin(parse_anthropic_sse(resp)))
    }
}

fn encode_anthropic_messages(messages: &[Message]) -> Vec<Value> {
    let mut encoded = Vec::<Value>::new();
    for message in messages {
        let (role, blocks) = match message {
            Message::System { .. } => continue,
            Message::User { content } => ("user", vec![json!({ "type": "text", "text": content })]),
            Message::Assistant {
                content,
                tool_calls,
            } => {
                let mut blocks = Vec::new();
                if !content.is_empty() {
                    blocks.push(json!({ "type": "text", "text": content }));
                }
                blocks.extend(tool_calls.iter().map(|call| {
                    let input = serde_json::from_str::<Value>(&call.arguments)
                        .unwrap_or_else(|_| json!({}));
                    json!({
                        "type": "tool_use",
                        "id": call.id,
                        "name": call.name,
                        "input": input,
                    })
                }));
                ("assistant", blocks)
            }
            Message::Tool { call_id, content } => (
                "user",
                vec![json!({
                    "type": "tool_result",
                    "tool_use_id": call_id,
                    "content": content,
                })],
            ),
        };
        if blocks.is_empty() {
            continue;
        }
        if let Some(last) = encoded.last_mut()
            && last["role"] == role
            && let Some(content) = last["content"].as_array_mut()
        {
            content.extend(blocks);
            continue;
        }
        encoded.push(json!({ "role": role, "content": blocks }));
    }
    encoded
}

fn parse_anthropic_sse(resp: reqwest::Response) -> impl futures_util::Stream<Item = Result<Delta>> {
    async_stream::try_stream! {
        let mut bytes = resp.bytes_stream();
        let mut buf = String::new();
        let mut pending: HashMap<u32, ToolCall> = HashMap::new();
        let mut usage = Usage::default();
        let mut reason = StopReason::Stop;

        'outer: while let Some(chunk) = bytes.next().await {
            buf.push_str(&String::from_utf8_lossy(&chunk?));
            while let Some((pos, delimiter_len)) = frame_boundary(&buf) {
                let frame: String = buf.drain(..pos + delimiter_len).collect();
                for line in frame.lines() {
                    let Some(data) = line.strip_prefix("data:") else { continue };
                    let data = data.trim();
                    let Ok(value) = serde_json::from_str::<Value>(data) else { continue };
                    match value["type"].as_str().unwrap_or_default() {
                        "message_start" => {
                            usage.input_tokens = value["message"]["usage"]["input_tokens"]
                                .as_i64()
                                .unwrap_or(0);
                            usage.output_tokens = value["message"]["usage"]["output_tokens"]
                                .as_i64()
                                .unwrap_or(0);
                        }
                        "content_block_start" if value["content_block"]["type"] == "tool_use" => {
                            let index = value["index"].as_u64().unwrap_or(0) as u32;
                            let input = &value["content_block"]["input"];
                            pending.insert(index, ToolCall {
                                id: value["content_block"]["id"]
                                    .as_str()
                                    .unwrap_or_default()
                                    .to_string(),
                                name: value["content_block"]["name"]
                                    .as_str()
                                    .unwrap_or_default()
                                    .to_string(),
                                arguments: if input.is_object()
                                    && input.as_object().is_some_and(|object| !object.is_empty())
                                {
                                    input.to_string()
                                } else {
                                    String::new()
                                },
                            });
                        }
                        "content_block_delta" => {
                            match value["delta"]["type"].as_str().unwrap_or_default() {
                                "text_delta" => {
                                    if let Some(text) = value["delta"]["text"].as_str()
                                        && !text.is_empty()
                                    {
                                        yield Delta::Text(text.to_string());
                                    }
                                }
                                "thinking_delta" => {
                                    if let Some(text) = value["delta"]["thinking"].as_str()
                                        && !text.is_empty()
                                    {
                                        yield Delta::Reasoning(text.to_string());
                                    }
                                }
                                "input_json_delta" => {
                                    let index = value["index"].as_u64().unwrap_or(0) as u32;
                                    if let Some(fragment) = value["delta"]["partial_json"].as_str() {
                                        pending.entry(index).or_insert_with(|| ToolCall {
                                            id: String::new(),
                                            name: String::new(),
                                            arguments: String::new(),
                                        }).arguments.push_str(fragment);
                                    }
                                }
                                _ => {}
                            }
                        }
                        "message_delta" => {
                            usage.output_tokens = value["usage"]["output_tokens"]
                                .as_i64()
                                .unwrap_or(usage.output_tokens);
                            reason = match value["delta"]["stop_reason"].as_str().unwrap_or_default() {
                                "tool_use" => StopReason::ToolCalls,
                                "max_tokens" => StopReason::Length,
                                _ => StopReason::Stop,
                            };
                        }
                        "error" => {
                            let message = value["error"]["message"]
                                .as_str()
                                .unwrap_or("unknown Anthropic stream error");
                            Err(anyhow::anyhow!("Anthropic stream error: {message}"))?;
                        }
                        "message_stop" => break 'outer,
                        _ => {}
                    }
                }
            }
        }

        let mut calls: Vec<_> = pending.into_iter().collect();
        calls.sort_by_key(|(index, _)| *index);
        let mut emitted = 0usize;
        for (_, mut call) in calls {
            if call.name.is_empty() {
                continue;
            }
            if call.id.is_empty() {
                call.id = uuid::Uuid::new_v4().to_string();
            }
            if call.arguments.trim().is_empty() {
                call.arguments = "{}".to_string();
            }
            emitted += 1;
            yield Delta::ToolCall(call);
        }
        if emitted > 0 {
            reason = StopReason::ToolCalls;
        }
        yield Delta::Done { reason, usage };
    }
}

// ---------------------------------------------------------------------------
// Official account-backed CLI
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
pub enum CliFlavor {
    Claude,
}

pub struct CliProvider {
    name: String,
    flavor: CliFlavor,
    workspace: PathBuf,
    sandbox: SandboxMode,
}

impl CliProvider {
    pub fn new(
        name: impl Into<String>,
        flavor: CliFlavor,
        workspace: impl Into<PathBuf>,
        sandbox: SandboxMode,
    ) -> Self {
        Self {
            name: name.into(),
            flavor,
            workspace: workspace.into(),
            sandbox,
        }
    }
}

#[async_trait]
impl Provider for CliProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn delegates_execution(&self) -> bool {
        true
    }

    async fn stream(&self, req: Request) -> Result<BoxStream<'static, Result<Delta>>> {
        let flavor = self.flavor;
        let workspace = self.workspace.clone();
        let sandbox = self.sandbox;
        let model = req.model.clone();
        let prompt = cli_prompt(&req.messages);

        Ok(Box::pin(async_stream::try_stream! {
            let (program, args) = cli_command(flavor, &workspace, sandbox, &model);
            let mut child = tokio::process::Command::new(&program)
                .args(&args)
                .current_dir(&workspace)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .with_context(|| cli_install_hint(flavor, &program))?;

            let mut stdin = child.stdin.take().context("cannot open provider CLI stdin")?;
            stdin.write_all(prompt.as_bytes()).await?;
            stdin.shutdown().await?;
            drop(stdin);

            let output = child.wait_with_output().await?;
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                Err(anyhow::anyhow!(
                    "{} exited with {}: {}",
                    program.display(),
                    output.status,
                    stderr.trim().chars().take(1000).collect::<String>()
                ))?;
            }
            let text = String::from_utf8(output.stdout)
                .context("provider CLI returned non-UTF-8 output")?;
            if !text.trim().is_empty() {
                yield Delta::Text(text);
            }
            yield Delta::Done {
                reason: StopReason::Stop,
                usage: Usage::default(),
            };
        }))
    }
}

fn cli_command(
    flavor: CliFlavor,
    _workspace: &Path,
    sandbox: SandboxMode,
    model: &str,
) -> (PathBuf, Vec<String>) {
    match flavor {
        CliFlavor::Claude => {
            let mut args = vec![
                "-p".to_string(),
                "--output-format".to_string(),
                "text".to_string(),
                "--no-session-persistence".to_string(),
            ];
            match sandbox {
                SandboxMode::ReadOnly => {
                    args.extend(["--permission-mode".to_string(), "plan".to_string()]);
                }
                SandboxMode::Normal | SandboxMode::FullAccess => {
                    args.push("--dangerously-skip-permissions".to_string());
                }
                SandboxMode::IsolatedWorkspaceWrite => {
                    args.extend(["--permission-mode".to_string(), "plan".to_string()]);
                }
            }
            if model != "default" {
                args.extend(["--model".to_string(), model.to_string()]);
            }
            (claude_executable(), args)
        }
    }
}

/// Resolve the official Claude Code CLI even when GnomeAI was launched from a
/// desktop entry whose PATH does not contain the user's local bin directory.
pub fn claude_executable() -> PathBuf {
    if let Some(path) = std::env::var_os("GNOMEF_CLAUDE_BIN").filter(|value| !value.is_empty()) {
        return PathBuf::from(path);
    }
    if let Some(path) = executable_in_path("claude") {
        return path;
    }

    let home = std::env::var_os("HOME").map(PathBuf::from);
    let npm_prefix = std::env::var_os("NPM_CONFIG_PREFIX").map(PathBuf::from);
    for candidate in claude_user_candidates(home.as_deref(), npm_prefix.as_deref()) {
        if candidate.is_file() {
            return candidate;
        }
    }
    PathBuf::from("claude")
}

fn executable_in_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file())
    })
}

fn claude_user_candidates(home: Option<&Path>, npm_prefix: Option<&Path>) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(home) = home {
        candidates.extend([
            home.join(".local/bin/claude"),
            home.join(".claude/bin/claude"),
            home.join(".claude/local/claude"),
            home.join(".npm-global/bin/claude"),
            home.join(".npm/bin/claude"),
        ]);
    }
    if let Some(prefix) = npm_prefix {
        candidates.push(prefix.join("bin/claude"));
    }
    candidates.extend([
        PathBuf::from("/usr/local/bin/claude"),
        PathBuf::from("/usr/bin/claude"),
    ]);
    candidates
}

/// Run and verify the vendor-owned Claude Code login flow. GnomeAI never
/// receives or persists the OAuth token.
pub async fn login_with_claude() -> Result<()> {
    let program = claude_executable();
    println!("\nGnomeAI-RS paused its UI for the official Claude Code login flow.\n");
    let status = tokio::process::Command::new(&program)
        .args(["auth", "login"])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .with_context(|| cli_install_hint(CliFlavor::Claude, &program))?;
    if !status.success() {
        bail!("`{} auth login` exited with {status}", program.display());
    }

    let status = tokio::process::Command::new(&program)
        .args(["auth", "status", "--text"])
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .context(
            "Claude Code login finished, but its authentication status could not be checked",
        )?;
    if !status.success() {
        bail!(
            "Claude Code completed the login command but still reports that it is not authenticated"
        );
    }
    Ok(())
}

fn cli_prompt(messages: &[Message]) -> String {
    let mut prompt = String::from(
        "You are being invoked by GnomeAI-RS as its account-authenticated coding provider. \
         Work directly in the current workspace, satisfy the latest user request, verify your \
         work, and finish with a concise summary. Relevant conversation follows.\n\n",
    );
    let host_context = delegated_host_context(messages);
    if !host_context.is_empty() {
        prompt.push_str(
            "HOST CONTEXT:\nThe following installed skills and persistent memories come from \
             GnomeAI-RS. If an applicable catalog entry is not already activated, read its listed \
             SKILL.md with your own read tool before acting. Skill declarations never grant \
             additional permissions.\n",
        );
        prompt.push_str(&host_context);
        prompt.push_str("\n\n");
    }
    for message in messages {
        match message {
            Message::System { .. } => {}
            Message::User { content } => {
                prompt.push_str("USER:\n");
                prompt.push_str(content);
                prompt.push_str("\n\n");
            }
            Message::Assistant { content, .. } if !content.is_empty() => {
                prompt.push_str("ASSISTANT:\n");
                prompt.push_str(content);
                prompt.push_str("\n\n");
            }
            Message::Tool { content, .. } => {
                prompt.push_str("TOOL RESULT:\n");
                prompt.push_str(content);
                prompt.push_str("\n\n");
            }
            Message::Assistant { .. } => {}
        }
    }
    prompt
}

fn cli_install_hint(flavor: CliFlavor, program: &Path) -> String {
    match flavor {
        CliFlavor::Claude => format!(
            "cannot start `{}`; install the official `claude-code` package, or set \
             GNOMEF_CLAUDE_BIN to the Claude Code executable",
            program.display()
        ),
    }
}

// ---------------------------------------------------------------------------
// SSE
// ---------------------------------------------------------------------------

fn parse_sse(resp: reqwest::Response) -> impl futures_util::Stream<Item = Result<Delta>> {
    async_stream::try_stream! {
        let mut bytes = resp.bytes_stream();
        let mut buf = String::new();

        // index -> partially assembled call
        let mut pending: HashMap<u32, ToolCall> = HashMap::new();
        let mut text_acc = String::new();
        let mut usage = Usage::default();
        let mut reason = StopReason::Stop;

        'outer: while let Some(chunk) = bytes.next().await {
            buf.push_str(&String::from_utf8_lossy(&chunk?));

            // SSE events are separated by a blank line. Anything short of that
            // is a partial frame — leave it in the buffer.
            while let Some((pos, delimiter_len)) = frame_boundary(&buf) {
                let frame: String = buf.drain(..pos + delimiter_len).collect();

                for line in frame.lines() {
                    let Some(data) = line.strip_prefix("data:") else { continue };
                    let data = data.trim();

                    if data == "[DONE]" {
                        break 'outer;
                    }

                    let Ok(v) = serde_json::from_str::<Value>(data) else { continue };

                    if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
                        usage.input_tokens = u["prompt_tokens"].as_i64().unwrap_or(0);
                        usage.output_tokens = u["completion_tokens"].as_i64().unwrap_or(0);
                    }

                    let Some(choice) = v["choices"].get(0) else { continue };

                    if let Some(fr) = choice["finish_reason"].as_str() {
                        reason = match fr {
                            "tool_calls" | "function_call" => StopReason::ToolCalls,
                            "length" => StopReason::Length,
                            _ => StopReason::Stop,
                        };
                    }

                    let delta = &choice["delta"];

                    // Some servers expose reasoning under different keys.
                    for key in ["reasoning_content", "reasoning"] {
                        if let Some(r) = delta[key].as_str() {
                            if !r.is_empty() {
                                yield Delta::Reasoning(r.to_string());
                            }
                        }
                    }

                    if let Some(t) = delta["content"].as_str() {
                        if !t.is_empty() {
                            text_acc.push_str(t);
                            yield Delta::Text(t.to_string());
                        }
                    }

                    // Fragment assembly. `index` is the only stable key; `id`
                    // and `name` arrive once and then never again.
                    if let Some(calls) = delta["tool_calls"].as_array() {
                        for c in calls {
                            let idx = c["index"].as_u64().unwrap_or(0) as u32;
                            let entry = pending.entry(idx).or_insert_with(|| ToolCall {
                                id: String::new(),
                                name: String::new(),
                                arguments: String::new(),
                            });

                            if let Some(id) = c["id"].as_str() {
                                if !id.is_empty() {
                                    entry.id = id.to_string();
                                }
                            }
                            if let Some(n) = c["function"]["name"].as_str() {
                                if !n.is_empty() {
                                    entry.name = n.to_string();
                                }
                            }
                            if let Some(a) = c["function"]["arguments"].as_str() {
                                entry.arguments.push_str(a);
                            }
                        }
                    }
                }
            }
        }

        // Emit assembled calls in index order — the model chose that order and
        // for sequential edits it matters.
        let mut assembled: Vec<(u32, ToolCall)> = pending.into_iter().collect();
        assembled.sort_by_key(|(i, _)| *i);

        let mut emitted = 0usize;
        for (_, mut call) in assembled {
            if call.name.is_empty() {
                continue;
            }
            if call.id.is_empty() {
                call.id = uuid::Uuid::new_v4().to_string();
            }
            if call.arguments.trim().is_empty() {
                call.arguments = "{}".to_string();
            }
            emitted += 1;
            yield Delta::ToolCall(call);
        }

        // Structured field was empty but the model may have written the call
        // into the prose. Common with local models on templates that never got
        // tool support wired up.
        if emitted == 0 {
            for call in salvage_tool_calls(&text_acc) {
                emitted += 1;
                yield Delta::ToolCall(call);
            }
        }

        if emitted > 0 {
            reason = StopReason::ToolCalls;
        }

        yield Delta::Done { reason, usage };
    }
}

fn frame_boundary(buffer: &str) -> Option<(usize, usize)> {
    match (buffer.find("\n\n"), buffer.find("\r\n\r\n")) {
        (Some(lf), Some(crlf)) if crlf < lf => Some((crlf, 4)),
        (Some(lf), _) => Some((lf, 2)),
        (None, Some(crlf)) => Some((crlf, 4)),
        (None, None) => None,
    }
}

/// Last-resort extraction of a tool call embedded in assistant text.
///
/// Looks for a fenced JSON object carrying `name` plus `arguments`. Deliberately
/// conservative: a false positive turns a normal answer into a phantom tool
/// call, which is worse than missing one.
fn salvage_tool_calls(text: &str) -> Vec<ToolCall> {
    let mut out = Vec::new();

    for block in text.split("```") {
        let body = block.trim_start_matches("json").trim();
        if !body.starts_with('{') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(body) else {
            continue;
        };

        let Some(name) = v["name"].as_str().or_else(|| v["tool"].as_str()) else {
            continue;
        };

        let args = v
            .get("arguments")
            .or_else(|| v.get("parameters"))
            .or_else(|| v.get("input"))
            .cloned()
            .unwrap_or_else(|| json!({}));

        out.push(ToolCall {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_string(),
            arguments: if args.is_string() {
                args.as_str().unwrap().to_string()
            } else {
                args.to_string()
            },
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn salvages_fenced_call() {
        let text =
            "I'll check.\n\n```json\n{\"name\":\"shell\",\"arguments\":{\"cmd\":\"ls\"}}\n```";
        let calls = salvage_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
    }

    #[test]
    fn ignores_plain_prose() {
        assert!(salvage_tool_calls("Here is what I found in the file.").is_empty());
    }

    #[test]
    fn delegated_context_keeps_skills_and_memory_but_not_arbitrary_system_text() {
        let messages = vec![
            Message::System {
                content: "internal provider secret".into(),
            },
            Message::System {
                content: "base\n--- Installed Agent Skills catalog ---\n- rust-review\n\
                          Cross-Conversation Memory\n- prefers Romanian"
                    .into(),
            },
            Message::System {
                content: "<activated_skill name=\"rust-review\">Review carefully</activated_skill>"
                    .into(),
            },
        ];
        let context = delegated_host_context(&messages);
        assert!(context.contains("rust-review"));
        assert!(context.contains("prefers Romanian"));
        assert!(context.contains("Review carefully"));
        assert!(!context.contains("internal provider secret"));
        assert!(!context.contains("base\n"));
    }

    #[test]
    fn accepts_lf_and_crlf_sse_boundaries() {
        assert_eq!(frame_boundary("data: one\n\nrest"), Some((9, 2)));
        assert_eq!(frame_boundary("data: one\r\n\r\nrest"), Some((9, 4)));
    }

    #[test]
    fn current_openai_models_use_modern_token_limit() {
        let mut provider = OpenAiCompatible::named("OpenAI", "https://api.openai.com/v1", None);
        provider.use_max_completion_tokens = true;
        let request = Request {
            model: "gpt-5.6-terra".into(),
            messages: vec![Message::User {
                content: "hello".into(),
            }],
            tools: Vec::new(),
            max_tokens: 123,
        };
        let body = provider.body(&request);
        assert_eq!(body["max_completion_tokens"], 123);
        assert!(body.get("max_tokens").is_none());
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn anthropic_encoding_preserves_tool_round_trip() {
        let messages = vec![
            Message::Assistant {
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call-1".into(),
                    name: "read_file".into(),
                    arguments: r#"{"path":"README.md"}"#.into(),
                }],
            },
            Message::Tool {
                call_id: "call-1".into(),
                content: "contents".into(),
            },
        ];
        let encoded = encode_anthropic_messages(&messages);
        assert_eq!(encoded[0]["content"][0]["type"], "tool_use");
        assert_eq!(encoded[0]["content"][0]["input"]["path"], "README.md");
        assert_eq!(encoded[1]["content"][0]["type"], "tool_result");
        assert_eq!(encoded[1]["content"][0]["tool_use_id"], "call-1");
    }

    #[test]
    fn cli_commands_map_sandbox_without_credentials() {
        let (_, claude) = cli_command(
            CliFlavor::Claude,
            Path::new("/tmp/project"),
            SandboxMode::ReadOnly,
            "default",
        );
        assert!(
            claude
                .windows(2)
                .any(|args| args == ["--permission-mode", "plan"])
        );
    }

    #[test]
    fn claude_candidates_cover_desktop_and_native_installs() {
        let candidates =
            claude_user_candidates(Some(Path::new("/home/adrian")), Some(Path::new("/opt/npm")));
        assert!(candidates.contains(&PathBuf::from("/home/adrian/.local/bin/claude")));
        assert!(candidates.contains(&PathBuf::from("/home/adrian/.claude/bin/claude")));
        assert!(candidates.contains(&PathBuf::from("/opt/npm/bin/claude")));
        assert!(candidates.contains(&PathBuf::from("/usr/bin/claude")));
    }
}

/// End-to-end provider tests against a local one-shot HTTP server: real
/// sockets, real reqwest, canned responses. Covers streaming, tool calling,
/// authentication headers, 401/429, and unknown models.
#[cfg(test)]
mod http_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// Accept one connection, read one full request, answer, hand the raw
    /// request text back for assertions.
    async fn one_shot_server(response: String) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 8192];
            loop {
                let n = socket.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..n]);
                if let Some(head_end) = find(&request, b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&request[..head_end]).to_string();
                    let content_length = head
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            if name.eq_ignore_ascii_case("content-length") {
                                value.trim().parse::<usize>().ok()
                            } else {
                                None
                            }
                        })
                        .unwrap_or(0);
                    if request.len() >= head_end + 4 + content_length {
                        break;
                    }
                }
            }
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.ok();
            String::from_utf8_lossy(&request).to_string()
        });
        (format!("http://{addr}/v1"), handle)
    }

    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    fn sse_response(frames: &[&str]) -> String {
        let body = frames
            .iter()
            .map(|frame| format!("data: {frame}\n\n"))
            .collect::<String>();
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}"
        )
    }

    fn error_response(status_line: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    fn request(model: &str) -> Request {
        Request {
            model: model.into(),
            messages: vec![Message::User {
                content: "salut".into(),
            }],
            tools: vec![ToolSpec {
                name: "shell".into(),
                description: "run a command".into(),
                parameters: json!({"type": "object"}),
            }],
            max_tokens: 128,
        }
    }

    async fn drain(
        mut stream: BoxStream<'static, Result<Delta>>,
    ) -> (String, Vec<ToolCall>, StopReason, Usage) {
        let mut text = String::new();
        let mut calls = Vec::new();
        let mut reason = StopReason::Stop;
        let mut usage = Usage::default();
        while let Some(delta) = stream.next().await {
            match delta.unwrap() {
                Delta::Text(t) => text.push_str(&t),
                Delta::ToolCall(call) => calls.push(call),
                Delta::Done {
                    reason: r,
                    usage: u,
                } => {
                    reason = r;
                    usage = u;
                }
                Delta::Reasoning(_) => {}
            }
        }
        (text, calls, reason, usage)
    }

    #[tokio::test]
    async fn openai_streams_text_sends_auth_and_reports_usage() {
        let (base, server) = one_shot_server(sse_response(&[
            r#"{"choices":[{"index":0,"delta":{"content":"Sal"}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":"ut"},"finish_reason":"stop"}],"usage":{"prompt_tokens":7,"completion_tokens":2}}"#,
            "[DONE]",
        ]))
        .await;

        let provider = OpenAiCompatible::named("test", base, Some("test-key".into()));
        let stream = provider.stream(request("test-model")).await.unwrap();
        let (text, calls, reason, usage) = drain(stream).await;

        assert_eq!(text, "Salut");
        assert!(calls.is_empty());
        assert_eq!(reason, StopReason::Stop);
        assert_eq!(usage.input_tokens, 7);
        assert_eq!(usage.output_tokens, 2);

        let raw = server.await.unwrap();
        let lowered = raw.to_lowercase();
        assert!(lowered.contains("authorization: bearer test-key"));
        assert!(raw.contains(r#""model":"test-model""#));
        assert!(raw.contains(r#""stream":true"#));
    }

    #[tokio::test]
    async fn openai_assembles_tool_call_fragments_across_chunks() {
        let (base, _server) = one_shot_server(sse_response(&[
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"shell","arguments":"{\"comm"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"and\":\"ls\"}"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#,
            "[DONE]",
        ]))
        .await;

        let provider = OpenAiCompatible::named("test", base, None);
        let stream = provider.stream(request("test-model")).await.unwrap();
        let (_, calls, reason, _) = drain(stream).await;

        assert_eq!(reason, StopReason::ToolCalls);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].name, "shell");
        assert_eq!(calls[0].arguments, r#"{"command":"ls"}"#);
    }

    #[tokio::test]
    async fn openai_401_bad_key_is_a_clear_error() {
        let (base, _server) = one_shot_server(error_response(
            "401 Unauthorized",
            r#"{"error":{"message":"Incorrect API key provided"}}"#,
        ))
        .await;

        let provider = OpenAiCompatible::named("test", base, Some("bad-key".into()));
        let error = provider
            .stream(request("test-model"))
            .await
            .err()
            .expect("401 must fail")
            .to_string();
        assert!(error.contains("401"), "missing status in: {error}");
        assert!(
            error.contains("Incorrect API key"),
            "missing body in: {error}"
        );
    }

    #[tokio::test]
    async fn openai_429_rate_limit_is_a_clear_error() {
        let (base, _server) = one_shot_server(error_response(
            "429 Too Many Requests",
            r#"{"error":{"message":"Rate limit reached, retry later"}}"#,
        ))
        .await;

        let provider = OpenAiCompatible::named("test", base, Some("key".into()));
        let error = provider
            .stream(request("test-model"))
            .await
            .err()
            .expect("429 must fail")
            .to_string();
        assert!(error.contains("429"), "missing status in: {error}");
        assert!(error.contains("Rate limit"), "missing body in: {error}");
    }

    #[tokio::test]
    async fn openai_unknown_model_surfaces_the_provider_message() {
        let (base, _server) = one_shot_server(error_response(
            "404 Not Found",
            r#"{"error":{"message":"The model `no-such-model` does not exist"}}"#,
        ))
        .await;

        let provider = OpenAiCompatible::named("test", base, Some("key".into()));
        let error = provider
            .stream(request("no-such-model"))
            .await
            .err()
            .expect("unknown model must fail")
            .to_string();
        assert!(error.contains("404"), "missing status in: {error}");
        assert!(error.contains("no-such-model"), "missing model in: {error}");
    }

    #[tokio::test]
    async fn openai_retries_once_without_stream_options() {
        // First request is answered 400 blaming stream_options; the retry
        // must succeed without the field.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let reject = error_response(
                "400 Bad Request",
                r#"{"error":{"message":"unknown field stream_options"}}"#,
            );
            let accept = sse_response(&[
                r#"{"choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":"stop"}]}"#,
                "[DONE]",
            ]);
            let mut second_body = String::new();
            for (index, response) in [reject, accept].into_iter().enumerate() {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut chunk = [0u8; 8192];
                loop {
                    let n = socket.read(&mut chunk).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..n]);
                    if let Some(head_end) = find(&request, b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&request[..head_end]).to_string();
                        let content_length = head
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())
                                    .flatten()
                            })
                            .unwrap_or(0);
                        if request.len() >= head_end + 4 + content_length {
                            break;
                        }
                    }
                }
                if index == 1 {
                    second_body = String::from_utf8_lossy(&request).to_string();
                }
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.shutdown().await.ok();
            }
            second_body
        });

        let provider = OpenAiCompatible::named("test", format!("http://{addr}/v1"), None);
        let stream = provider.stream(request("test-model")).await.unwrap();
        let (text, _, reason, _) = drain(stream).await;
        assert_eq!(text, "ok");
        assert_eq!(reason, StopReason::Stop);

        let second = server.await.unwrap();
        assert!(!second.contains("stream_options"));
    }

    #[tokio::test]
    async fn anthropic_streams_text_and_tool_use() {
        let (base, server) = one_shot_server(sse_response(&[
            r#"{"type":"message_start","message":{"usage":{"input_tokens":11,"output_tokens":1}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Verific."}}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"shell","input":{}}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\":"}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"ls\"}"}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":9}}"#,
            r#"{"type":"message_stop"}"#,
        ]))
        .await;

        let provider = Anthropic::new("anthropic", base, "sk-ant-test");
        let stream = provider.stream(request("claude-x")).await.unwrap();
        let (text, calls, reason, usage) = drain(stream).await;

        assert_eq!(text, "Verific.");
        assert_eq!(reason, StopReason::ToolCalls);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "toolu_1");
        assert_eq!(calls[0].name, "shell");
        assert_eq!(calls[0].arguments, r#"{"command":"ls"}"#);
        assert_eq!(usage.input_tokens, 11);
        assert_eq!(usage.output_tokens, 9);

        let raw = server.await.unwrap();
        let lowered = raw.to_lowercase();
        assert!(lowered.contains("x-api-key: sk-ant-test"));
        assert!(lowered.contains("anthropic-version:"));
    }

    #[tokio::test]
    async fn anthropic_401_bad_key_is_a_clear_error() {
        let (base, _server) = one_shot_server(error_response(
            "401 Unauthorized",
            r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#,
        ))
        .await;

        let provider = Anthropic::new("anthropic", base, "wrong");
        let error = provider
            .stream(request("claude-x"))
            .await
            .err()
            .expect("401 must fail")
            .to_string();
        assert!(error.contains("401"), "missing status in: {error}");
        assert!(
            error.contains("invalid x-api-key"),
            "missing body in: {error}"
        );
    }
}
