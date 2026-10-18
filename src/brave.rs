//! HTTP-only web tools, shared by Android and desktop.
use crate::{
    config::AppConfig,
    firecrawl::FirecrawlBundle,
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
        let result = brave_search_bundle(&config, query, mode);
        tokio::select! {
            _ = cancel.cancelled() => bail!("Search interrupted"),
            result = result => result,
        }
        .map(|bundle| ToolOutcome {
            content: bundle.text,
            ok: true,
            touched: vec![],
            patches: vec![],
        })
    }
}

/// Runs a Brave query and shapes the answer like a Firecrawl bundle so the
/// mobile `web_search` tool can treat both providers identically. `mode` is
/// `web` (snippet results) or `context` (research bundle).
pub async fn brave_search_bundle(
    config: &AppConfig,
    query: &str,
    mode: &str,
) -> Result<FirecrawlBundle> {
    if !config.web_search_enabled {
        bail!("Web search is disabled in Settings");
    }
    if config.brave_api_key.trim().is_empty() {
        bail!("Configure a Brave Search API key in Settings");
    }
    let query = query.trim();
    if query.is_empty() || query.chars().count() > 400 {
        bail!("Search query must contain 1–400 characters");
    }
    {
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
            Ok(bundle_from_brave(mode, value))
        };
        result.await
    }
}

/// Converts a Brave response into the shared bundle shape. `web` answers
/// become one entry per result so auto-search can rank them; `context`
/// answers stay one research entry because Brave already fused the sources.
fn bundle_from_brave(mode: &str, value: Value) -> FirecrawlBundle {
    if mode == "web" {
        let entries = value["web"]["results"]
            .as_array()
            .map(|results| {
                results
                    .iter()
                    .filter_map(|result| {
                        let url = result["url"].as_str()?.to_string();
                        Some(crate::firecrawl::FirecrawlEntry {
                            title: result["title"].as_str().unwrap_or("").to_string(),
                            description: result["description"]
                                .as_str()
                                .unwrap_or("")
                                .to_string(),
                            content: result["description"].as_str().unwrap_or("").to_string(),
                            url,
                        })
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if entries.is_empty() {
            return FirecrawlBundle {
                entries: Vec::new(),
                text: "No results found.".into(),
            };
        }
        let mut text = String::new();
        for entry in &entries {
            text.push_str(&format!(
                "**{}**\n{}\n{}\n\n",
                if entry.title.trim().is_empty() {
                    "Untitled"
                } else {
                    entry.title.trim()
                },
                entry.url,
                entry.description
            ));
        }
        return FirecrawlBundle {
            entries,
            text: text.trim().to_string(),
        };
    }
    let text = serde_json::to_string(&value).unwrap_or_default();
    let empty = text == "{}" || text.trim().is_empty();
    FirecrawlBundle {
        entries: if empty {
            Vec::new()
        } else {
            vec![crate::firecrawl::FirecrawlEntry {
                title: "Brave research context".into(),
                url: String::new(),
                description: String::new(),
                content: text.clone(),
            }]
        },
        text: if empty {
            "No results found.".into()
        } else {
            text
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn brave_web_results_become_bundle_entries() {
        let bundle = bundle_from_brave(
            "web",
            json!({"web": {"results": [
                {"title": "Firecrawl", "url": "https://firecrawl.dev", "description": "Scrape."},
                {"title": "", "url": "https://example.org", "description": "Fallback page."}
            ]}}),
        );
        assert_eq!(bundle.entries.len(), 2);
        assert_eq!(bundle.entries[0].url, "https://firecrawl.dev");
        assert!(bundle.text.contains("**Firecrawl**"));
        assert!(bundle.text.contains("https://example.org"));
        assert!(!bundle.text.starts_with("[Firecrawl"));
    }

    #[test]
    fn brave_web_without_results_reports_empty() {
        let bundle = bundle_from_brave("web", json!({"web": {"results": []}}));
        assert!(bundle.entries.is_empty());
        assert_eq!(bundle.text, "No results found.");
    }

    #[test]
    fn brave_context_stays_single_research_entry() {
        let bundle = bundle_from_brave("context", json!({"context": "fused research text"}));
        assert_eq!(bundle.entries.len(), 1);
        assert!(bundle.entries[0].content.contains("fused research text"));
        assert_eq!(bundle.text, bundle.entries[0].content);
    }

    #[test]
    fn entries_without_urls_are_dropped() {
        let bundle = bundle_from_brave(
            "web",
            json!({"web": {"results": [{"title": "no url", "description": "x"}]}}),
        );
        assert!(bundle.entries.is_empty());
    }
}
