//! HTTP-only web tools, shared by Android and desktop.
use crate::{
    config::AppConfig,
    provider::ToolSpec,
    tooling::{Tool, ToolDefinition, ToolOutcome},
};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

pub struct BraveTool {
    pub config: Arc<RwLock<AppConfig>>,
}

#[async_trait::async_trait]
impl Tool for BraveTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::network_read(ToolSpec { name:"web_search".into(),
            description:"Search the web with Brave. Returns source URLs and extracted context. Use mode web for snippets or context for research.".into(),
            parameters:json!({"type":"object","properties":{"query":{"type":"string"},"mode":{"type":"string","enum":["web","context"]}},"required":["query"]}) })
    }
    async fn call(&self, args: Value, cancel: &CancellationToken) -> Result<ToolOutcome> {
        let config = self.config.read().await.clone();
        if !config.web_search_enabled {
            bail!("Web search is disabled in Settings");
        }
        if config.brave_api_key.trim().is_empty() {
            bail!("Configure a Brave Search API key in Settings");
        }
        let query = args["query"].as_str().unwrap_or("").trim();
        if query.is_empty() || query.chars().count() > 400 {
            bail!("Search query must contain 1–400 characters");
        }
        let mode = args["mode"].as_str().unwrap_or(&config.brave_search_mode);
        let endpoint = if mode == "web" {
            "web/search"
        } else {
            "llm/context"
        };
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;
        let request = client
            .get(format!("https://api.search.brave.com/res/v1/{endpoint}"))
            .header("X-Subscription-Token", &config.brave_api_key)
            .query(&[("q", query)]);
        let request = if mode == "web" {
            request.query(&[("count", 5)])
        } else {
            request.query(&[
                ("maximum_number_of_tokens", 8192),
                ("maximum_number_of_urls", 8),
            ])
        };
        let result = async {
            let mut response = request.send().await?.error_for_status()?;
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await? {
                if body.len() + chunk.len() > 2 * 1024 * 1024 {
                    bail!("Brave response exceeded 2 MiB");
                }
                body.extend_from_slice(&chunk);
            }
            let value: Value = serde_json::from_slice(&body)?;
            let text = serde_json::to_string(&value)?;
            Ok(ToolOutcome {
                content: text,
                ok: true,
                touched: vec![],
                patches: vec![],
            })
        };
        tokio::select! { _ = cancel.cancelled() => bail!("Search interrupted"), result = result => result }
    }
}
