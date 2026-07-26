use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::RwLock;
use tracing::info;

use crate::{
    config::AppConfig,
    consistency::enforce_final_answer,
    firecrawl::{build_release_answer, firecrawl_search},
    llama::{LlamaClient, ModelInfo},
    memory::{MemoryStore, append_memory_block, build_working_memory_block},
    questions::PendingQuestions,
    runtime::RuntimeHandles,
    runtime_profile::{RuntimeProfile, build_runtime_aware_system_prompt},
    search::{is_release_query, should_auto_search},
    storage::{AppPaths, ChatMessage, build_context},
    tools::run_tool_loop,
    vision::{
        SYSTEM_PROMPT, build_image_vision_messages, find_extracted_content, find_image_path,
        generate_image_response, supports_images,
    },
};

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

pub async fn generate_chat_response_with_uploads(
    client: &LlamaClient,
    cfg: &AppConfig,
    paths: &AppPaths,
    memory_state: Option<Arc<RwLock<MemoryStore>>>,
    runtime_profile: &RuntimeProfile,
    model: &str,
    known_models: &[ModelInfo],
    query: &str,
    history: &[ChatMessage],
    session_key: Option<&str>,
    pending_questions: &PendingQuestions,
    runtime: &RuntimeHandles,
    config_state: Option<Arc<RwLock<AppConfig>>>,
) -> String {
    let memory_block = load_memory_block(
        memory_state.clone(),
        paths,
        cfg,
        query,
        history,
        session_key,
    )
    .await;
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
        runtime,
        config_state,
    )
    .await
}

pub async fn prepare_streaming_response_with_uploads(
    client: &LlamaClient,
    cfg: &AppConfig,
    paths: &AppPaths,
    memory_state: Option<Arc<RwLock<MemoryStore>>>,
    runtime_profile: &RuntimeProfile,
    model: &str,
    known_models: &[ModelInfo],
    query: &str,
    history: &[ChatMessage],
    session_key: Option<&str>,
    pending_questions: &PendingQuestions,
    runtime: &RuntimeHandles,
    config_state: Option<Arc<RwLock<AppConfig>>>,
) -> StreamResponsePlan {
    let memory_block = load_memory_block(
        memory_state.clone(),
        paths,
        cfg,
        query,
        history,
        session_key,
    )
    .await;
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
            runtime,
            config_state,
        )
        .await,
    )
}

pub async fn generate_standard_chat_response(
    client: &LlamaClient,
    cfg: &AppConfig,
    paths: &AppPaths,
    _memory_state: Option<Arc<RwLock<MemoryStore>>>,
    runtime_profile: &RuntimeProfile,
    memory_block: Option<&str>,
    model: &str,
    query: &str,
    history: &[ChatMessage],
    session_key: Option<&str>,
    pending_questions: &PendingQuestions,
    runtime: &RuntimeHandles,
    config_state: Option<Arc<RwLock<AppConfig>>>,
) -> String {
    let ctx = build_context(history, cfg.history_window);
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
        runtime,
        config_state,
        0,
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
    memory_state: Option<Arc<RwLock<MemoryStore>>>,
    paths: &AppPaths,
    cfg: &AppConfig,
    query: &str,
    history: &[ChatMessage],
    chat_id: Option<&str>,
) -> Option<String> {
    let memory_state = memory_state?;
    build_working_memory_block(&memory_state, paths, cfg, query, history, chat_id)
        .await
        .ok()
        .filter(|item| !item.trim().is_empty())
}

pub fn chunk_text_for_sse(text: &str, size: usize) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        current.push(ch);
        if current.len() >= size {
            chunks.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}
