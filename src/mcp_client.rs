//! Generic Model Context Protocol client integration.
//!
//! Every discovered MCP tool is adapted to GnomeAI's native `Tool` trait, so
//! providers see one registry and the agent keeps one approval path. MCP
//! annotations are treated as hints only; external calls always remain
//! approval-gated, even when full-access is selected.

use anyhow::{Context, Result, bail};
use rmcp::{
    RoleClient, ServiceExt,
    model::{CallToolRequestParams, Tool as McpTool},
    service::{RunningService, ServerSink},
    transport::{
        ConfigureCommandExt, StreamableHttpClientTransport, TokioChildProcess,
        streamable_http_client::StreamableHttpClientTransportConfig,
    },
};
use serde_json::{Map, Value};
use std::collections::{BTreeSet, HashMap};
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::config::{AppConfig, McpServerConfig, McpTransport};
use crate::provider::ToolSpec;
use crate::tooling::{
    ApprovalRequirement, Registry, Tool, ToolConcurrency, ToolDefinition, ToolEffect, ToolOutcome,
};

pub struct McpRuntime {
    /// Dropping a RunningService closes its transport. Keep every connection
    /// alive for exactly as long as its registered tool adapters are live.
    _connections: Vec<RunningService<RoleClient, ()>>,
    notices: Vec<String>,
}

impl McpRuntime {
    pub fn empty() -> Self {
        Self {
            _connections: Vec::new(),
            notices: Vec::new(),
        }
    }

    pub fn notices(&self) -> &[String] {
        &self.notices
    }
}

/// Connect every enabled server independently. A broken optional integration
/// never prevents the native agent from starting or rebuilding its registry.
pub async fn register_configured(registry: &mut Registry, config: &AppConfig) -> McpRuntime {
    let mut runtime = McpRuntime::empty();
    let mut names = BTreeSet::new();

    for server in config.mcp_servers.iter().filter(|server| server.enabled) {
        let mut connection = match connect(server).await {
            Ok(connection) => connection,
            Err(error) => {
                runtime.notices.push(format!(
                    "MCP `{}` could not connect: {error:#}",
                    server.name
                ));
                continue;
            }
        };

        let tools = match connection.list_all_tools().await {
            Ok(tools) => tools,
            Err(error) => {
                runtime.notices.push(format!(
                    "MCP `{}` connected but tools/list failed: {error}",
                    server.name
                ));
                let _ = connection.close().await;
                continue;
            }
        };

        let peer = connection.peer().clone();
        let mut registered = 0usize;
        for upstream in tools {
            let base = exposed_name(&server.name, upstream.name.as_ref());
            let mut exposed = base.clone();
            let mut suffix = 2usize;
            while names.contains(&exposed) || registry.contains(&exposed) {
                exposed = format!("{base}_{suffix}");
                suffix += 1;
            }
            names.insert(exposed.clone());
            registry.register(Arc::new(McpToolAdapter::new(
                server.name.clone(),
                exposed,
                upstream,
                peer.clone(),
            )));
            registered += 1;
        }
        runtime.notices.push(format!(
            "MCP `{}` connected with {registered} tool(s)",
            server.name
        ));
        runtime._connections.push(connection);
    }

    runtime
}

async fn connect(server: &McpServerConfig) -> Result<RunningService<RoleClient, ()>> {
    match server.transport {
        McpTransport::StreamableHttp => {
            if server.url.is_empty() {
                bail!("Streamable HTTP URL is empty");
            }
            let headers = server
                .headers
                .iter()
                .map(|(name, value)| {
                    let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                        .with_context(|| format!("invalid HTTP header name `{name}`"))?;
                    let value = reqwest::header::HeaderValue::from_str(value)
                        .with_context(|| format!("invalid value for HTTP header `{name}`"))?;
                    Ok((name, value))
                })
                .collect::<Result<HashMap<_, _>>>()?;
            let transport = StreamableHttpClientTransport::from_config(
                StreamableHttpClientTransportConfig::with_uri(server.url.clone())
                    .custom_headers(headers),
            );
            ().serve(transport)
                .await
                .with_context(|| format!("cannot initialize {}", server.url))
        }
        McpTransport::Stdio => {
            if server.command.is_empty() {
                bail!("stdio command is empty");
            }
            let transport =
                TokioChildProcess::new(Command::new(&server.command).configure(|command| {
                    command
                        .args(&server.args)
                        .envs(&server.env)
                        .stderr(Stdio::inherit())
                        .kill_on_drop(true);
                }))
                .with_context(|| format!("cannot start MCP command `{}`", server.command))?;
            ().serve(transport)
                .await
                .with_context(|| format!("cannot initialize MCP command `{}`", server.command))
        }
    }
}

struct McpToolAdapter {
    definition: ToolDefinition,
    upstream_name: String,
    peer: ServerSink,
}

impl McpToolAdapter {
    fn new(server_name: String, exposed_name: String, tool: McpTool, peer: ServerSink) -> Self {
        let read_only = tool
            .annotations
            .as_ref()
            .and_then(|annotations| annotations.read_only_hint)
            == Some(true);
        let destructive = !read_only
            && tool
                .annotations
                .as_ref()
                .and_then(|annotations| annotations.destructive_hint)
                .unwrap_or(true);
        let mut effects = if read_only {
            vec![ToolEffect::ExternalRead]
        } else {
            vec![ToolEffect::ExternalWrite]
        };
        if destructive {
            effects.push(ToolEffect::ExternalDestructive);
        }
        let description = tool.description.as_deref().unwrap_or("MCP tool");
        let definition = ToolDefinition {
            spec: ToolSpec {
                name: exposed_name,
                description: format!("MCP `{server_name}`: {description}"),
                parameters: Value::Object((*tool.input_schema).clone()),
            },
            effects,
            concurrency: if read_only {
                ToolConcurrency::Parallel
            } else {
                ToolConcurrency::Exclusive
            },
            approval: ApprovalRequirement::External,
        };
        Self {
            definition,
            upstream_name: tool.name.into_owned(),
            peer,
        }
    }
}

#[async_trait::async_trait]
impl Tool for McpToolAdapter {
    fn definition(&self) -> ToolDefinition {
        self.definition.clone()
    }

    async fn call(&self, args: Value, cancel: &CancellationToken) -> Result<ToolOutcome> {
        let arguments = match args {
            Value::Object(arguments) => arguments,
            Value::Null => Map::new(),
            _ => bail!("MCP tool arguments must be a JSON object"),
        };
        let request =
            CallToolRequestParams::new(self.upstream_name.clone()).with_arguments(arguments);
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => bail!("MCP tool call cancelled"),
            result = self.peer.call_tool(request) => result.context("MCP tools/call failed")?,
        };
        let ok = !result.is_error.unwrap_or(false);
        Ok(ToolOutcome {
            content: render_result(&result),
            ok,
            touched: Vec::new(),
            patches: Vec::new(),
        })
    }
}

fn exposed_name(server: &str, tool: &str) -> String {
    format!("mcp_{}_{}", identifier(server), identifier(tool))
}

fn identifier(value: &str) -> String {
    let mut output = String::new();
    let mut separator = false;
    for character in value.to_ascii_lowercase().chars() {
        if character.is_ascii_alphanumeric() {
            output.push(character);
            separator = false;
        } else if !separator && !output.is_empty() {
            output.push('_');
            separator = true;
        }
    }
    while output.ends_with('_') {
        output.pop();
    }
    if output.is_empty() {
        "tool".into()
    } else {
        output
    }
}

fn render_result(result: &rmcp::model::CallToolResult) -> String {
    let mut parts = Vec::new();
    for block in &result.content {
        let value = serde_json::to_value(block).unwrap_or(Value::Null);
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("content");
        match kind {
            "text" => {
                if let Some(text) = value.get("text").and_then(Value::as_str) {
                    parts.push(text.to_string());
                }
            }
            "resource" => {
                if let Some(text) = value.pointer("/resource/text").and_then(Value::as_str) {
                    parts.push(text.to_string());
                } else if let Some(uri) = value.pointer("/resource/uri").and_then(Value::as_str) {
                    parts.push(format!("[MCP resource: {uri}]"));
                }
            }
            "resource_link" => {
                let uri = value
                    .get("uri")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                parts.push(format!("[MCP resource link: {uri}]"));
            }
            "image" | "audio" => {
                let mime = value
                    .get("mimeType")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                parts.push(format!("[MCP {kind}: {mime}]"));
            }
            _ => parts.push(serde_json::to_string(&value).unwrap_or_default()),
        }
    }
    if let Some(structured) = &result.structured_content {
        let structured =
            serde_json::to_string_pretty(structured).unwrap_or_else(|_| structured.to_string());
        parts.push(format!("[structured content]\n{structured}"));
    }
    if parts.is_empty() {
        "(empty MCP result)".into()
    } else {
        parts.join("\n\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_names_are_namespaced_and_model_safe() {
        assert_eq!(
            exposed_name("Browser OS", "browser.click-element"),
            "mcp_browser_os_browser_click_element"
        );
        assert_eq!(exposed_name("", ""), "mcp_tool_tool");
    }
}
