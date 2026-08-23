use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::{
    config::AppConfig,
    consistency::enforce_final_answer,
    firecrawl::{build_release_answer, firecrawl_search},
    llama::{LlamaClient, ModelInfo},
    memory::append_memory_block,
    memory_engine::MemoryEngine,
    questions::PendingQuestions,
    runtime::RuntimeHandles,
    runtime_profile::{RuntimeProfile, build_runtime_aware_system_prompt},
    search::{is_release_query, should_auto_search},
    skills,
    storage::{AppPaths, ChatMessage},
    tools::run_tool_loop,
    turn_stream::TurnStream,
    vision::{
        SYSTEM_PROMPT, build_image_vision_messages, find_extracted_content, find_image_path,
        generate_image_response, supports_images,
    },
    web_approvals::PendingApprovals,
};

// Context compaction design adapted from Turnstone (Apache-2.0):
// https://github.com/turnstonelabs/turnstone
// The transcript remains untouched; only the model-facing context is compacted.
const AUTO_COMPACT_PCT: usize = 80;
const HARD_CONTEXT_PCT: usize = 90;
const RECENT_MESSAGES_TO_KEEP: usize = 8;
const DEFAULT_CONTEXT_WINDOW_TOKENS: usize = 128_000;
const CHARS_PER_TOKEN_ESTIMATE: usize = 4;
const MAX_CONTEXT_MESSAGE_CHARS: usize = 16_000;
const MAX_COMPACTION_DEPTH: usize = 4;

const CONTEXT_COMPACTOR_SYSTEM_PROMPT: &str = r#"# Conversation Compactor

Your output replaces the older portion of a conversation. The assistant will continue from your summary without seeing those old messages.

Use these exact sections when relevant:
## Decisions
Choices made about architecture, libraries, approaches, settings, or behavior.
## Files
Files read, created, or modified, with exact paths and brief notes.
## Key code
Exact function names, type names, variables, commands, identifiers, endpoints, model names, and short code fragments needed to continue. Never paraphrase identifiers.
## Tool results
Important tool outputs, errors that remain relevant, search findings, and observed runtime behavior.
## Open tasks
Unfinished work and the user's current request, with enough detail to continue immediately.
## User preferences
Explicit workflow preferences, constraints, and instructions stated by the user.

Rules:
- Be dense. Every token should carry useful state.
- Preserve exact paths, identifiers, numbers, commands, model names, and version numbers.
- Drop greetings, acknowledgements, jokes, duplicated text, and dead ends that were later resolved.
- If an error was later fixed, keep the fix rather than the obsolete failure.
- Do not invent facts or infer preferences that were not explicitly stated.
- This is a continuation checkpoint, not a prose recap."#;

pub enum StreamResponsePlan {
    Direct {
        messages: Vec<Value>,
        temperature: f64,
    },
    Fallback(String),
}

pub async fn resolve_model(
    client: &LlamaClient,
    cfg: &AppConfig,
    requested: Option<&str>,
) -> (String, Vec<ModelInfo>) {
    let preferred = requested
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .unwrap_or(&cfg.default_model);
    let models = client.list_models(cfg).await.unwrap_or_default();
    if models.iter().any(|info| info.id == preferred) {
        return (preferred.to_string(), models);
    }
    if let Some(first) = models.first() {
        return (first.id.clone(), models);
    }
    (preferred.to_string(), models)
}

fn configured_context_window_tokens(model: &str) -> usize {
    if let Ok(value) = std::env::var("GNOMEF_CONTEXT_WINDOW_TOKENS") {
        if let Ok(tokens) = value.trim().parse::<usize>() {
            if tokens >= 8_192 {
                return tokens.clamp(8_192, 2_000_000);
            }
        }
    }

    let model = model.to_ascii_lowercase();
    if model.contains("gemini") || model.contains("kimi") {
        256_000
    } else if model.contains("gpt-5")
        || model.contains("claude")
        || model.contains("deepseek")
        || model.contains("qwen")
        || model.contains("grok")
        || model.contains("gemma")
    {
        128_000
    } else {
        DEFAULT_CONTEXT_WINDOW_TOKENS
    }
}

fn estimate_tokens(text: &str) -> usize {
    text.chars()
        .count()
        .div_ceil(CHARS_PER_TOKEN_ESTIMATE)
        .max(1)
}

fn truncate_middle(text: &str, max_chars: usize) -> String {
    let chars = text.chars().collect::<Vec<_>>();
    if chars.len() <= max_chars {
        return text.to_string();
    }
    let marker = "\n...[truncated for context budgeting]...\n";
    let marker_len = marker.chars().count();
    let usable = max_chars.saturating_sub(marker_len);
    let head = usable * 2 / 3;
    let tail = usable.saturating_sub(head);
    format!(
        "{}{}{}",
        chars[..head].iter().collect::<String>(),
        marker,
        chars[chars.len().saturating_sub(tail)..]
            .iter()
            .collect::<String>()
    )
}

fn context_message(message: &ChatMessage) -> Option<String> {
    let role = match message.role.trim().to_ascii_lowercase().as_str() {
        "user" => "User",
        "assistant" | "gnome" => "Assistant",
        _ => return None,
    };

    match &message.content {
        Value::String(text) => {
            if text.starts_with("[Extracted content from uploaded file:") {
                return None;
            }
            Some(format!(
                "{role}: {}",
                truncate_middle(text, MAX_CONTEXT_MESSAGE_CHARS)
            ))
        }
        Value::Object(obj) => {
            let file_type = obj.get("type").and_then(Value::as_str).unwrap_or("unknown");
            let filename = obj
                .get("filename")
                .and_then(Value::as_str)
                .unwrap_or("file");
            Some(format!("User: [uploaded {file_type} file: {filename}]"))
        }
        other => Some(format!(
            "{role}: {}",
            truncate_middle(&other.to_string(), MAX_CONTEXT_MESSAGE_CHARS)
        )),
    }
}

fn chunk_blocks(blocks: &[String], max_chars: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();

    for block in blocks {
        let block = truncate_middle(block, max_chars);
        let extra = if current.is_empty() { 0 } else { 2 };
        if !current.is_empty() && current.chars().count() + extra + block.chars().count() > max_chars
        {
            chunks.push(std::mem::take(&mut current));
        }
        if !current.is_empty() {
            current.push_str("\n\n");
        }
        current.push_str(&block);
    }

    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

async fn summarize_context_chunk(
    client: &LlamaClient,
    cfg: &AppConfig,
    model: &str,
    text: &str,
) -> anyhow::Result<String> {
    let response = client
        .chat(
            cfg,
            model,
            vec![
                json!({"role": "system", "content": CONTEXT_COMPACTOR_SYSTEM_PROMPT}),
                json!({
                    "role": "user",
                    "content": format!("Compact the following conversation checkpoint:\n\n{text}")
                }),
            ],
            0.1,
        )
        .await?;
    let summary = response.content.trim();
    if summary.is_empty() {
        anyhow::bail!("context compactor returned an empty summary");
    }
    Ok(summary.to_string())
}

async fn summarize_context_recursive(
    client: &LlamaClient,
    cfg: &AppConfig,
    model: &str,
    blocks: Vec<String>,
    context_window: usize,
) -> anyhow::Result<String> {
    let input_char_budget = (context_window * CHARS_PER_TOKEN_ESTIMATE * 45 / 100).max(16_000);
    let mut current = blocks;

    for depth in 0..MAX_COMPACTION_DEPTH {
        let chunks = chunk_blocks(&current, input_char_budget);
        let mut summaries = Vec::with_capacity(chunks.len());
        for chunk in chunks {
            summaries.push(summarize_context_chunk(client, cfg, model, &chunk).await?);
        }

        let merged = summaries.join("\n\n");
        let target_tokens = context_window * 22 / 100;
        if summaries.len() == 1 || estimate_tokens(&merged) <= target_tokens {
            return Ok(merged);
        }

        info!(
            depth,
            input_tokens = estimate_tokens(&merged),
            "Context summary still large; recursively compacting"
        );
        current = summaries;
    }

    Ok(truncate_middle(
        &current.join("\n\n"),
        context_window * CHARS_PER_TOKEN_ESTIMATE / 4,
    ))
}

async fn build_managed_context(
    client: &LlamaClient,
    cfg: &AppConfig,
    model: &str,
    history: &[ChatMessage],
) -> String {
    let messages = history
        .iter()
        .filter_map(context_message)
        .collect::<Vec<_>>();
    if messages.is_empty() {
        return "(empty)".into();
    }

    let raw = messages.join("\n\n");
    let context_window = configured_context_window_tokens(model);
    let used = estimate_tokens(&raw);
    let soft_limit = context_window * AUTO_COMPACT_PCT / 100;
    if used <= soft_limit {
        return raw;
    }

    let keep = RECENT_MESSAGES_TO_KEEP.min(messages.len());
    let split = messages.len().saturating_sub(keep);
    if split == 0 {
        return raw;
    }

    let old = messages[..split].to_vec();
    let recent = messages[split..].join("\n\n");
    info!(
        used_tokens = used,
        context_window,
        soft_limit,
        compacting_messages = old.len(),
        keeping_messages = keep,
        "Automatic context compaction triggered"
    );

    match summarize_context_recursive(client, cfg, model, old, context_window).await {
        Ok(summary) => {
            let compacted = format!(
                "[Earlier conversation compacted automatically]\n{summary}\n\n[Recent conversation kept verbatim]\n{recent}"
            );
            let compacted_tokens = estimate_tokens(&compacted);
            if compacted_tokens > context_window * HARD_CONTEXT_PCT / 100 {
                warn!(
                    compacted_tokens,
                    context_window,
                    "Compacted context remains near the hard context ceiling"
                );
            }
            compacted
        }
        Err(error) => {
            warn!(%error, "Automatic context compaction failed; using bounded recent context");
            let fallback_budget = context_window * CHARS_PER_TOKEN_ESTIMATE * 70 / 100;
            truncate_middle(&raw, fallback_budget)
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn generate_chat_response_with_uploads(
    client: &LlamaClient,
    cfg: &AppConfig,
    paths: &AppPaths,
    memory_state: Option<Arc<MemoryEngine>>,
    runtime_profile: &RuntimeProfile,
    model: &str,
    known_models: &[ModelInfo],
    query: &str,
    history: &[ChatMessage],
    session_key: Option<&str>,
    pending_questions: &PendingQuestions,
    pending_approvals: &PendingApprovals,
    runtime: &RuntimeHandles,
    config_state: Option<Arc<RwLock<AppConfig>>>,
    local_web: bool,
    turn: &TurnStream,
) -> String {
    let memory_block =
        load_memory_block(memory_state.clone(), cfg, query, history, session_key).await;
    let memory_block = combine_context_blocks(memory_block, skill_catalog_context(paths));
    let memory_block = combine_context_blocks(memory_block, active_skill_context(history));
    let (extracted, file_type, filename) = find_extracted_content(history, query);
    if file_type.as_deref() == Some("image") {
        return generate_image_response(
            client,
            cfg,
            runtime_profile,
            memory_block.as_deref(),
            model,
            known_models,
            query,
            history,
            extracted,
            filename,
        )
        .await;
    }

    if let Some(content) = extracted.as_ref().filter(|text| !text.trim().is_empty()) {
        let prompt = format!(
            "The user uploaded a file called '{}'.\n\nFile content:\n{}\n\nUser's question: {}",
            filename.unwrap_or_else(|| "file".into()),
            content,
            query
        );
        return call_plain(
            client,
            cfg,
            runtime_profile,
            memory_block.as_deref(),
            model,
            &prompt,
        )
        .await;
    }

    generate_standard_chat_response(
        client,
        cfg,
        paths,
        memory_state,
        runtime_profile,
        memory_block.as_deref(),
        model,
        query,
        history,
        session_key,
        pending_questions,
        pending_approvals,
        runtime,
        config_state,
        local_web,
        turn,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn prepare_streaming_response_with_uploads(
    client: &LlamaClient,
    cfg: &AppConfig,
    paths: &AppPaths,
    memory_state: Option<Arc<MemoryEngine>>,
    runtime_profile: &RuntimeProfile,
    model: &str,
    known_models: &[ModelInfo],
    query: &str,
    history: &[ChatMessage],
    session_key: Option<&str>,
    pending_questions: &PendingQuestions,
    pending_approvals: &PendingApprovals,
    runtime: &RuntimeHandles,
    config_state: Option<Arc<RwLock<AppConfig>>>,
    local_web: bool,
    turn: &TurnStream,
) -> StreamResponsePlan {
    let memory_block =
        load_memory_block(memory_state.clone(), cfg, query, history, session_key).await;
    let memory_block = combine_context_blocks(memory_block, skill_catalog_context(paths));
    let memory_block = combine_context_blocks(memory_block, active_skill_context(history));
    let (extracted, file_type, filename) = find_extracted_content(history, query);
    if file_type.as_deref() == Some("image") {
        let (image_path, image_name_from_history) = find_image_path(history, filename.as_deref());
        let image_name = if !image_name_from_history.is_empty() {
            image_name_from_history
        } else {
            filename.clone().unwrap_or_else(|| "image".into())
        };
        if let Some(path) = image_path.as_ref() {
            if supports_images(model, known_models) {
                let system_prompt = append_memory_block(
                    &build_runtime_aware_system_prompt(SYSTEM_PROMPT, runtime_profile),
                    memory_block.as_deref(),
                );
                if let Ok(messages) =
                    build_image_vision_messages(&system_prompt, query, &image_name, path)
                {
                    return StreamResponsePlan::Direct {
                        messages,
                        temperature: 0.2,
                    };
                }
            }
        }
        return StreamResponsePlan::Fallback(
            generate_image_response(
                client,
                cfg,
                runtime_profile,
                memory_block.as_deref(),
                model,
                known_models,
                query,
                history,
                extracted,
                filename,
            )
            .await,
        );
    }

    if let Some(content) = extracted.as_ref().filter(|text| !text.trim().is_empty()) {
        let prompt = format!(
            "The user uploaded a file called '{}'.\n\nFile content:\n{}\n\nUser's question: {}",
            filename.unwrap_or_else(|| "file".into()),
            content,
            query
        );
        let system_prompt = append_memory_block(
            &build_runtime_aware_system_prompt(SYSTEM_PROMPT, runtime_profile),
            memory_block.as_deref(),
        );
        return StreamResponsePlan::Direct {
            messages: vec![
                json!({"role": "system", "content": system_prompt}),
                json!({"role": "user", "content": prompt}),
            ],
            temperature: 0.7,
        };
    }

    StreamResponsePlan::Fallback(
        generate_standard_chat_response(
            client,
            cfg,
            paths,
            memory_state,
            runtime_profile,
            memory_block.as_deref(),
            model,
            query,
            history,
            session_key,
            pending_questions,
            pending_approvals,
            runtime,
            config_state,
            local_web,
            turn,
        )
        .await,
    )
}

#[allow(clippy::too_many_arguments)]
pub async fn generate_standard_chat_response(
    client: &LlamaClient,
    cfg: &AppConfig,
    paths: &AppPaths,
    _memory_state: Option<Arc<MemoryEngine>>,
    runtime_profile: &RuntimeProfile,
    memory_block: Option<&str>,
    model: &str,
    query: &str,
    history: &[ChatMessage],
    session_key: Option<&str>,
    pending_questions: &PendingQuestions,
    pending_approvals: &PendingApprovals,
    runtime: &RuntimeHandles,
    config_state: Option<Arc<RwLock<AppConfig>>>,
    local_web: bool,
    turn: &TurnStream,
) -> String {
    let ctx = build_managed_context(client, cfg, model, history).await;
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let decision = should_auto_search(query, history);
    let mut prompt = format!("Today's date: {today}\nConversation:\n{ctx}\n\n");

    if cfg.web_search_enabled && decision.search {
        let web_bundle = firecrawl_search(cfg, &decision.query).await;
        info!(
            "Auto-search[{}/{}]: '{}'",
            decision.score,
            decision.reason,
            decision.query.chars().take(80).collect::<String>()
        );
        if let Some(answer) = build_release_answer(&decision.query, &web_bundle.entries) {
            return answer;
        }
        prompt.push_str(
            "Use the web results below as the source of truth for factual and current claims. \
Do not invent details that are not present in the results. If the results are incomplete or \
conflicting, say so clearly and cite the source names or URLs you used. Prefer official or \
first-party sources over blogs whenever they are present in the results. Only mention dates, \
titles, relationships, version numbers, or other concrete facts if they are explicitly stated \
in the provided results.\n\n",
        );
        if is_release_query(&decision.query) {
            prompt.push_str(
                "This is a software/version lookup. Answer briefly with the current version and \
official links only. Do not mention a release date unless the exact date appears in the provided \
web results; otherwise omit the date.\n\n",
            );
        }
        prompt.push_str(&format!(
            "Web search results for '{}':\n{}\n\n",
            decision.query, web_bundle.text
        ));
    } else {
        info!(
            "Auto-search skipped[{}/{}]: '{}'",
            decision.score,
            decision.reason,
            query.chars().take(80).collect::<String>()
        );
    }

    prompt.push_str(&format!("User: {query}"));
    run_tool_loop(
        client,
        cfg,
        paths,
        model,
        SYSTEM_PROMPT,
        runtime_profile,
        memory_block,
        &prompt,
        session_key,
        pending_questions,
        pending_approvals,
        runtime,
        config_state,
        0,
        local_web,
        turn,
    )
    .await
}

async fn call_plain(
    client: &LlamaClient,
    cfg: &AppConfig,
    runtime_profile: &RuntimeProfile,
    memory_block: Option<&str>,
    model: &str,
    prompt: &str,
) -> String {
    let system_prompt = append_memory_block(
        &build_runtime_aware_system_prompt(SYSTEM_PROMPT, runtime_profile),
        memory_block,
    );
    let messages = vec![
        json!({"role": "system", "content": system_prompt}),
        json!({"role": "user", "content": prompt}),
    ];
    match client.chat(cfg, model, messages, 0.7).await {
        Ok(response) if !response.content.trim().is_empty() => {
            enforce_final_answer(&response.content, runtime_profile, &[])
        }
        Ok(_) => "[Empty response]".into(),
        Err(err) => format!("[LLM error: {err}]"),
    }
}

async fn load_memory_block(
    memory_state: Option<Arc<MemoryEngine>>,
    cfg: &AppConfig,
    query: &str,
    history: &[ChatMessage],
    chat_id: Option<&str>,
) -> Option<String> {
    let engine = memory_state?;
    engine
        .working_memory_block(cfg, query, history, chat_id)
        .await
        .ok()
        .filter(|item| !item.trim().is_empty())
}

fn active_skill_context(history: &[ChatMessage]) -> Option<String> {
    let mut blocks = Vec::new();
    for message in history {
        if message.role != "system"
            || message.extra.get("type").and_then(Value::as_str) != Some("skill_activation")
        {
            continue;
        }
        if let Some(content) = message.content.as_str() {
            blocks.push(content.to_string());
        }
    }
    (!blocks.is_empty()).then(|| {
        format!(
            "--- Active Agent Skills (instructions) ---\n{}",
            blocks.join("\n\n")
        )
    })
}

fn skill_catalog_context(paths: &AppPaths) -> Option<String> {
    let catalog = skills::web_catalog_prompt(&paths.workspace_dir);
    (!catalog.trim().is_empty()).then_some(catalog)
}

fn combine_context_blocks(first: Option<String>, second: Option<String>) -> Option<String> {
    match (first, second) {
        (Some(first), Some(second)) => Some(format!("{first}\n\n{second}")),
        (Some(first), None) => Some(first),
        (None, Some(second)) => Some(second),
        (None, None) => None,
    }
}

#[cfg(test)]
mod context_compaction_tests {
    use super::*;
    use chrono::Utc;

    fn msg(role: &str, text: &str) -> ChatMessage {
        ChatMessage {
            role: role.into(),
            content: Value::String(text.into()),
            timestamp: Utc::now(),
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn context_messages_preserve_full_recent_text_beyond_old_500_char_limit() {
        let text = "x".repeat(2_000);
        let rendered = context_message(&msg("user", &text)).unwrap();
        assert!(rendered.len() > 1_500);
    }

    #[test]
    fn context_ignores_skill_system_messages() {
        assert!(context_message(&msg("system", "secret skill prompt")).is_none());
    }

    #[test]
    fn chunker_respects_budget_for_normal_blocks() {
        let blocks = vec!["a".repeat(100), "b".repeat(100), "c".repeat(100)];
        let chunks = chunk_blocks(&blocks, 210);
        assert_eq!(chunks.len(), 2);
        assert!(chunks.iter().all(|chunk| chunk.chars().count() <= 210));
    }
}
