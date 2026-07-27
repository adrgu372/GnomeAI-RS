use std::{collections::HashSet, path::PathBuf, process::Stdio, time::Duration};

use anyhow::anyhow;
use regex::Regex;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::config::AppConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FirecrawlEntry {
    pub title: String,
    pub url: String,
    pub description: String,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct FirecrawlBundle {
    pub entries: Vec<FirecrawlEntry>,
    pub text: String,
}

pub async fn firecrawl_search(cfg: &AppConfig, query: &str) -> FirecrawlBundle {
    if !cfg.web_search_enabled {
        return FirecrawlBundle {
            entries: Vec::new(),
            text: "[Firecrawl: Web Search is disabled.]".into(),
        };
    }
    if firecrawl_base_url(cfg).is_empty() {
        return FirecrawlBundle {
            entries: Vec::new(),
            text: "[Firecrawl: No API URL. Set it in Settings.]".into(),
        };
    }

    let client = reqwest::Client::new();
    let result: anyhow::Result<FirecrawlBundle> = async {
        ensure_local_firecrawl(&client, cfg).await?;
        let response = firecrawl_post(
            &client,
            cfg,
            "/search",
            json!({
                "query": query,
                "limit": cfg.firecrawl_count,
                "sources": ["web"],
            }),
            Duration::from_secs(30),
        )
        .await?;
        let results = prioritize_search_results(query, extract_search_results(&response));
        if results.is_empty() {
            return Ok(FirecrawlBundle {
                entries: Vec::new(),
                text: "No results found.".into(),
            });
        }

        let entries = enrich_results(&client, cfg, results).await;
        let text = render_results(&entries);
        Ok(FirecrawlBundle {
            entries,
            text: if text.trim().is_empty() {
                "No results found.".into()
            } else {
                text
            },
        })
    }
    .await;

    match result {
        Ok(bundle) => bundle,
        Err(err) => FirecrawlBundle {
            entries: Vec::new(),
            text: format!("[Firecrawl error: {err}]"),
        },
    }
}

pub async fn firecrawl_fetch(cfg: &AppConfig, url: &str) -> FirecrawlEntry {
    let client = reqwest::Client::new();
    if !cfg.web_search_enabled {
        return firecrawl_error_entry(url, "Web Search is disabled");
    }
    if let Err(error) = ensure_local_firecrawl(&client, cfg).await {
        return firecrawl_error_entry(url, &error.to_string());
    }
    scrape_result(&client, cfg, url).await
}

/// Serialises lazy startup process-wide: with parallel searches in flight,
/// exactly one caller runs the launcher while the rest wait and re-check.
static FIRECRAWL_LAUNCH: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn ensure_local_firecrawl(client: &reqwest::Client, cfg: &AppConfig) -> anyhow::Result<()> {
    // Callers gate on `web_search_enabled` before reaching this point, so a
    // disabled switch can never start containers. Keep the invariant here too.
    if !cfg.web_search_enabled {
        return Err(anyhow!("Web Search is disabled"));
    }
    let base = firecrawl_base_url(cfg);
    if !is_local_firecrawl_url(&base) {
        return Ok(());
    }
    if firecrawl_is_reachable(client, &base).await {
        return Ok(());
    }

    let _launch_guard = FIRECRAWL_LAUNCH.lock().await;
    // Someone else may have finished the launch while we waited for the lock.
    if firecrawl_is_reachable(client, &base).await {
        return Ok(());
    }

    let launcher = firecrawl_launcher().ok_or_else(|| {
        anyhow!(
            "local Firecrawl is not running and the lazy-start launcher was not found; \
             install the packaged `gnomeai-firecrawl` script or point `firecrawl_api_url` \
             to a running Firecrawl instance"
        )
    })?;
    if !command_in_path("podman") {
        return Err(anyhow!(
            "Podman is not installed, so the packaged rootless Firecrawl cannot start. \
             Install it (e.g. `sudo apt install podman`) or set `firecrawl_api_url` to a \
             hosted Firecrawl endpoint"
        ));
    }
    let output = tokio::process::Command::new(&launcher)
        .arg("ensure")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|error| anyhow!("cannot run {}: {error}", launcher.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut detail: String = stderr.trim().chars().take(500).collect();
        if detail.is_empty() {
            detail = "no error output".into();
        }
        return Err(anyhow!(
            "local Firecrawl could not start: {detail} — check `gnomeai-firecrawl logs` and \
             that rootless Podman works for this user (`podman info`)"
        ));
    }

    for _ in 0..60 {
        if firecrawl_is_reachable(client, &base).await {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Err(anyhow!(
        "local Firecrawl did not become ready at {base} within 60 seconds; \
         inspect `gnomeai-firecrawl status` and `gnomeai-firecrawl logs`"
    ))
}

pub fn command_in_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| {
                let candidate = dir.join(name);
                candidate.is_file()
            })
        })
        .unwrap_or(false)
}

async fn firecrawl_is_reachable(client: &reqwest::Client, base: &str) -> bool {
    client
        .get(base)
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .is_ok()
}

fn is_local_firecrawl_url(base: &str) -> bool {
    let lower = base.to_ascii_lowercase();
    lower.starts_with("http://127.0.0.1:")
        || lower.starts_with("http://localhost:")
        || lower.starts_with("http://[::1]:")
}

fn firecrawl_launcher() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("GNOMEF_FIRECRAWL_LAUNCHER") {
        let path = PathBuf::from(path);
        if path.is_file() {
            if let Ok(path) = path.canonicalize() {
                return Some(path);
            }
        }
    }

    let mut candidates = vec![
        PathBuf::from("/usr/lib/gnomeai-rs/firecrawl/gnomeai-firecrawl"),
        PathBuf::from("scripts/gnomeai-firecrawl"),
    ];
    if let Ok(executable) = std::env::current_exe()
        && let Some(parent) = executable.parent()
    {
        candidates.push(parent.join("firecrawl").join("gnomeai-firecrawl"));
        candidates.push(parent.join("..").join("scripts").join("gnomeai-firecrawl"));
    }
    candidates
        .into_iter()
        .filter(|path| path.is_file())
        .find_map(|path| path.canonicalize().ok())
}

fn firecrawl_error_entry(url: &str, message: &str) -> FirecrawlEntry {
    FirecrawlEntry {
        title: String::new(),
        url: url.to_string(),
        description: String::new(),
        content: format!("[Firecrawl error: {message}]"),
    }
}

pub fn build_release_answer(query: &str, entries: &[FirecrawlEntry]) -> Option<String> {
    if !is_release_query(query) || entries.is_empty() {
        return None;
    }

    let tokens = search_query_tokens(query);
    let version_re = Regex::new(r"\b\d+\.\d+(?:\.\d+){0,2}\b").unwrap();
    let unstable_re =
        Regex::new(r"(?i)\b(future|development|alpha|beta|rc|pre-release|pre release|preview)\b")
            .unwrap();
    let mut candidates: Vec<(i32, i32, Vec<u32>, String, String)> = Vec::new();

    for entry in entries {
        let text = format!("{} {} {}", entry.title, entry.description, entry.content);
        let versions = version_re
            .find_iter(&text)
            .map(|m| m.as_str().to_string())
            .collect::<Vec<_>>();
        if versions.is_empty() {
            continue;
        }

        let host = host_from_url(&entry.url);
        let priority = if tokens.iter().any(|token| host.contains(token)) {
            2
        } else if text.to_lowercase().contains("official") {
            1
        } else {
            0
        };
        let stability = if unstable_re.is_match(&text) { -1 } else { 0 };
        let best_version = versions
            .into_iter()
            .max_by_key(|version| version_sort_key(version))
            .unwrap_or_default();
        candidates.push((
            priority,
            stability,
            version_sort_key(&best_version),
            best_version,
            entry.url.clone(),
        ));
    }

    if candidates.is_empty() {
        return None;
    }
    candidates.sort_by(|a, b| b.cmp(a));
    let latest = candidates[0].3.clone();
    let mut urls = Vec::new();
    for (_, _, _, _, url) in candidates {
        if !url.trim().is_empty() && !urls.contains(&url) {
            urls.push(url);
        }
        if urls.len() >= 3 {
            break;
        }
    }

    let mut answer = format!("Cea mai probabila versiune curenta este `{latest}`.");
    if !urls.is_empty() {
        answer.push_str("\n\nSurse:\n");
        for url in urls {
            answer.push_str(&format!("- {url}\n"));
        }
    }
    Some(answer.trim().to_string())
}

fn firecrawl_base_url(cfg: &AppConfig) -> String {
    cfg.firecrawl_api_url
        .trim()
        .trim_end_matches('/')
        .to_string()
}

fn firecrawl_headers(cfg: &AppConfig) -> anyhow::Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    let api_key = cfg.firecrawl_api_key.trim();
    if !api_key.is_empty() {
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {api_key}"))?,
        );
    }
    Ok(headers)
}

fn firecrawl_candidate_urls(cfg: &AppConfig, path: &str) -> Vec<String> {
    let base = firecrawl_base_url(cfg);
    if base.is_empty() {
        return Vec::new();
    }
    if base.ends_with(path) {
        return vec![base];
    }
    if base.ends_with("/v1") || base.ends_with("/v2") {
        return vec![format!("{base}{path}")];
    }

    let candidates = vec![
        format!("{base}/v2{path}"),
        format!("{base}/v1{path}"),
        format!("{base}{path}"),
    ];
    let mut seen = HashSet::new();
    candidates
        .into_iter()
        .filter(|url| seen.insert(url.clone()))
        .collect()
}

async fn firecrawl_post(
    client: &reqwest::Client,
    cfg: &AppConfig,
    path: &str,
    payload: Value,
    timeout: Duration,
) -> anyhow::Result<Value> {
    let mut last_error = "unknown error".to_string();
    for url in firecrawl_candidate_urls(cfg, path) {
        let response = client
            .post(&url)
            .headers(firecrawl_headers(cfg)?)
            .json(&payload)
            .timeout(timeout)
            .send()
            .await;
        let response = match response {
            Ok(response) => response,
            Err(err) => {
                last_error = format!("{url}: {err}");
                continue;
            }
        };
        if response.status().as_u16() == 404 {
            last_error = format!("{url}: HTTP 404");
            continue;
        }
        if response.status().is_success() {
            return response
                .json::<Value>()
                .await
                .map_err(|err| anyhow!("{url}: invalid JSON: {err}"));
        }

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        let message = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|value| {
                value
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| body.chars().take(300).collect::<String>());
        last_error = format!("{url}: HTTP {status} {message}");
    }
    Err(anyhow!(last_error))
}

fn extract_search_results(response: &Value) -> Vec<Map<String, Value>> {
    let Some(obj) = response.as_object() else {
        return Vec::new();
    };

    if let Some(data) = obj.get("data") {
        if let Some(data_obj) = data.as_object() {
            for key in ["web", "results"] {
                let results = normalize_result_list(data_obj.get(key));
                if !results.is_empty() {
                    return results;
                }
            }
        }
        let results = normalize_result_list(Some(data));
        if !results.is_empty() {
            return results;
        }
    }

    for key in ["web", "results", "items"] {
        let results = normalize_result_list(obj.get(key));
        if !results.is_empty() {
            return results;
        }
    }
    Vec::new()
}

fn normalize_result_list(value: Option<&Value>) -> Vec<Map<String, Value>> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_object().cloned())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

async fn enrich_results(
    client: &reqwest::Client,
    cfg: &AppConfig,
    results: Vec<Map<String, Value>>,
) -> Vec<FirecrawlEntry> {
    let limit = cfg.firecrawl_extract_count.min(results.len() as u32) as usize;
    let mut scraped = std::collections::HashMap::new();

    for result in results.iter().take(limit) {
        let url = result_url(result);
        if url.is_empty() || scraped.contains_key(&url) {
            continue;
        }
        scraped.insert(url.clone(), scrape_result(client, cfg, &url).await);
    }

    results
        .into_iter()
        .take(cfg.firecrawl_count as usize)
        .map(|result| {
            let url = result_url(&result);
            let scraped_data = scraped.get(&url);
            let title = compact_ws(value_text(&result, &["title", "name"]))
                .or_else(|| scraped_data.map(|item| item.title.clone()))
                .unwrap_or_default();
            let description = strip_html_excerpt(
                &value_text(&result, &["description", "snippet"]).unwrap_or_default(),
            );
            let content = scraped_data
                .map(|item| item.content.clone())
                .filter(|text| !text.trim().is_empty())
                .unwrap_or_else(|| {
                    truncate_web_excerpt(
                        cfg,
                        &value_text(&result, &["markdown", "content", "html"]).unwrap_or_default(),
                    )
                });

            FirecrawlEntry {
                title,
                url: scraped_data
                    .map(|item| item.url.clone())
                    .filter(|text| !text.trim().is_empty())
                    .unwrap_or(url),
                description,
                content,
            }
        })
        .collect()
}

async fn scrape_result(client: &reqwest::Client, cfg: &AppConfig, url: &str) -> FirecrawlEntry {
    let response = firecrawl_post(
        client,
        cfg,
        "/scrape",
        json!({
            "url": url,
            "formats": ["markdown", "html"],
            "onlyMainContent": true,
            "removeBase64Images": true,
            "timeout": cfg.firecrawl_timeout_ms,
            "blockAds": true,
        }),
        Duration::from_secs(60),
    )
    .await;

    let Ok(response) = response else {
        let error = response.unwrap_err();
        return firecrawl_error_entry(url, &error.to_string());
    };
    let payload = extract_scrape_payload(&response);
    let metadata = payload
        .get("metadata")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let final_url = metadata
        .get("sourceURL")
        .or_else(|| metadata.get("url"))
        .and_then(Value::as_str)
        .unwrap_or(url)
        .to_string();
    let title = metadata
        .get("title")
        .or_else(|| payload.get("title"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    FirecrawlEntry {
        title,
        url: final_url,
        description: String::new(),
        content: content_from_payload(cfg, &payload),
    }
}

fn extract_scrape_payload(response: &Value) -> Map<String, Value> {
    if let Some(data) = response.get("data").and_then(Value::as_object) {
        return data.clone();
    }
    response.as_object().cloned().unwrap_or_default()
}

fn content_from_payload(cfg: &AppConfig, payload: &Map<String, Value>) -> String {
    let content = value_text(payload, &["markdown", "content"]).unwrap_or_default();
    if !content.trim().is_empty() {
        return truncate_web_excerpt(cfg, &content);
    }
    let html = value_text(payload, &["html", "rawHtml"]).unwrap_or_default();
    truncate_web_excerpt(cfg, &strip_html_excerpt(&html))
}

fn render_results(entries: &[FirecrawlEntry]) -> String {
    entries
        .iter()
        .map(|entry| {
            let mut lines = vec![format!(
                "**{}**",
                if entry.title.trim().is_empty() {
                    "Untitled"
                } else {
                    entry.title.trim()
                }
            )];
            if !entry.url.trim().is_empty() {
                lines.push(entry.url.clone());
            }
            if !entry.description.trim().is_empty() {
                lines.push(entry.description.clone());
            }
            if !entry.content.trim().is_empty() {
                lines.push(format!("Content excerpt:\n{}", entry.content));
            }
            lines.join("\n")
        })
        .filter(|block| !block.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn result_url(result: &Map<String, Value>) -> String {
    value_text(result, &["url", "link", "sourceURL"]).unwrap_or_default()
}

fn value_text(obj: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(text) = obj.get(*key).and_then(Value::as_str) {
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn compact_ws(text: Option<String>) -> Option<String> {
    text.map(|value| normalize_ws(&value))
        .filter(|value| !value.is_empty())
}

fn normalize_ws(text: &str) -> String {
    Regex::new(r"\s+")
        .unwrap()
        .replace_all(text, " ")
        .trim()
        .to_string()
}

fn clean_base64_images(text: &str) -> String {
    let text = Regex::new(r"\(data:image/[^;]+;base64,[A-Za-z0-9+/=]+\)")
        .unwrap()
        .replace_all(text, "[BASE64_IMAGE_REMOVED]")
        .to_string();
    Regex::new(r"data:image/[^;]+;base64,[A-Za-z0-9+/=]+")
        .unwrap()
        .replace_all(&text, "[BASE64_IMAGE_REMOVED]")
        .to_string()
}

fn strip_html_excerpt(html: &str) -> String {
    normalize_ws(&Regex::new(r"<[^>]+>").unwrap().replace_all(html, " "))
}

fn truncate_web_excerpt(cfg: &AppConfig, text: &str) -> String {
    let cleaned = clean_base64_images(text).trim().to_string();
    if cleaned.len() <= cfg.firecrawl_excerpt_chars {
        return cleaned;
    }
    format!(
        "{}...",
        cleaned
            .chars()
            .take(cfg.firecrawl_excerpt_chars)
            .collect::<String>()
            .trim_end()
    )
}

fn prioritize_search_results(
    query: &str,
    results: Vec<Map<String, Value>>,
) -> Vec<Map<String, Value>> {
    if !is_release_query(query) {
        return results;
    }
    let tokens = search_query_tokens(query);
    if tokens.is_empty() {
        return results;
    }
    let preferred = results
        .iter()
        .filter(|result| {
            let host = host_from_url(&result_url(result));
            tokens.iter().any(|token| host.contains(token))
        })
        .cloned()
        .collect::<Vec<_>>();
    if preferred.is_empty() {
        results
    } else {
        preferred
    }
}

fn is_release_query(query: &str) -> bool {
    let lowered = query.to_lowercase();
    [
        "version",
        "release",
        "download",
        "update",
        "installer",
        "changelog",
        "versiune",
        "lansare",
        "descarc",
        "actualizare",
    ]
    .iter()
    .any(|needle| lowered.contains(needle))
}

fn search_query_tokens(query: &str) -> Vec<String> {
    let stopwords: HashSet<&str> = [
        "the",
        "a",
        "an",
        "and",
        "or",
        "for",
        "of",
        "in",
        "on",
        "latest",
        "current",
        "official",
        "release",
        "version",
        "download",
        "update",
        "installer",
        "changelog",
        "notes",
        "what",
        "is",
        "tell",
        "me",
        "about",
        "who",
    ]
    .into_iter()
    .collect();
    Regex::new(r"[a-z0-9]+")
        .unwrap()
        .find_iter(&query.to_lowercase())
        .map(|m| m.as_str().to_string())
        .filter(|token| token.len() > 2 && !stopwords.contains(token.as_str()))
        .collect()
}

fn version_sort_key(version: &str) -> Vec<u32> {
    version
        .split('.')
        .filter_map(|part| part.parse::<u32>().ok())
        .collect()
}

fn host_from_url(url: &str) -> String {
    let without_scheme = url.split("://").nth(1).unwrap_or(url);
    without_scheme
        .split('/')
        .next()
        .unwrap_or("")
        .trim_start_matches("www.")
        .to_lowercase()
}
