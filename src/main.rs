mod chat_logic;
mod config;
mod consistency;
mod firecrawl;
mod generation;
mod llama;
mod memory;
mod questions;
mod runtime;
mod runtime_profile;
mod search;
mod storage;
mod tasks;
mod tools;
mod uploads;
mod vision;
mod whatsapp;

use std::{
    collections::HashSet, convert::Infallible, fs, net::SocketAddr, path::PathBuf, sync::Arc,
};

use async_stream::stream;
use axum::{
    Json, Router,
    extract::{Multipart, Path as AxumPath, Query, State},
    http::{StatusCode, header::CONTENT_TYPE},
    response::{
        Html, IntoResponse, Response,
        sse::{Event, Sse},
    },
    routing::{get, post},
};
use futures_util::{Stream, StreamExt};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::sync::{Mutex, RwLock};
use tower_http::{cors::CorsLayer, trace::TraceLayer};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::{
    chat_logic::{
        StreamResponsePlan, chunk_text_for_sse, generate_chat_response_with_uploads,
        prepare_streaming_response_with_uploads, resolve_model,
    },
    config::AppConfig,
    generation::{detect_format, generate_document},
    llama::LlamaClient,
    memory::{MemoryStore, refresh_memory_from_chat, update_recent_memory_snapshot},
    questions::PendingQuestions,
    runtime::RuntimeHandles,
    runtime_profile::RuntimeProfile,
    storage::{
        AppPaths, append_msg, clear_chat, delete_chat, image_meta, list_chats, load_chat, new_chat,
        save_chat,
    },
    tasks as workflow_tasks,
    uploads::{read_file, sanitize_upload_filename, save_base64_image, unique_upload_path},
    whatsapp::{WhatsAppBridge, qr_svg, self_whatsapp_jid, send_whatsapp_message},
};

#[derive(Clone)]
struct AppState {
    paths: Arc<AppPaths>,
    config: Arc<RwLock<AppConfig>>,
    memory: Arc<RwLock<MemoryStore>>,
    runtime_profile: Arc<RuntimeProfile>,
    llama: LlamaClient,
    pending_questions: PendingQuestions,
    runtime: RuntimeHandles,
    whatsapp: WhatsAppBridge,
    wa_seen: Arc<Mutex<HashSet<String>>>,
}

#[derive(Debug)]
struct AppError(anyhow::Error);

impl<E> From<E> for AppError
where
    E: Into<anyhow::Error>,
{
    fn from(value: E) -> Self {
        Self(value.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        error!("{:?}", self.0);
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok": false, "error": self.0.to_string()})),
        )
            .into_response()
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("gnomef_rs=info".parse()?))
        .init();

    let app_dir = std::env::var("GNOMEF_RS_HOME")
        .map(PathBuf::from)
        .unwrap_or(std::env::current_dir()?);
    let paths = Arc::new(AppPaths::new(app_dir)?);
    let memory = Arc::new(RwLock::new(MemoryStore::load(paths.as_ref())?));
    let runtime_profile = Arc::new(RuntimeProfile::detect(paths.as_ref()));
    let config = AppConfig::load(&paths.config_file)?;

    let bind_addr: SocketAddr = format!("{}:{}", config.host, config.port).parse()?;
    let state = AppState {
        paths,
        config: Arc::new(RwLock::new(config)),
        memory,
        runtime_profile,
        llama: LlamaClient::new(),
        pending_questions: PendingQuestions::default(),
        runtime: RuntimeHandles::default(),
        whatsapp: WhatsAppBridge::new(),
        wa_seen: Arc::new(Mutex::new(HashSet::new())),
    };

    {
        let cfg = state.config.read().await.clone();
        if cfg.whatsapp_enabled {
            match state.whatsapp.start(&cfg, &state.paths, false).await {
                Ok(msg) => info!("WhatsApp bridge {msg}"),
                Err(err) => warn!("WhatsApp bridge failed to start: {err}"),
            }
        }
    }

    let shutdown_state = state.clone();
    let app = Router::new()
        .route("/", get(index))
        .route("/api/config", get(api_get_config).post(api_set_config))
        .route("/api/memory", get(api_get_memory))
        .route("/api/runtime/profile", get(api_runtime_profile))
        .route("/api/models", get(api_models))
        .route("/api/chats", get(api_list_chats))
        .route("/api/chats/new", post(api_new_chat))
        .route(
            "/api/chats/{cid}",
            get(api_get_chat).delete(api_delete_chat),
        )
        .route("/api/chats/{cid}/clear", post(api_clear_chat))
        .route("/api/chats/{cid}/messages", post(api_add_message))
        .route("/api/chats/{cid}/start-stream", post(api_start_stream))
        .route(
            "/api/chats/{cid}/pending-question",
            get(api_get_pending_question),
        )
        .route(
            "/api/chats/{cid}/pending-question/answer",
            post(api_answer_pending_question),
        )
        .route("/api/chats/{cid}/tasks", get(api_chat_tasks))
        .route("/api/tasks/{task_id}/stop", post(api_stop_task))
        .route(
            "/api/runtime/tasks/{task_id}/update",
            post(api_runtime_task_update),
        )
        .route("/api/upload/{cid}", post(api_upload))
        .route("/api/file/{filename}", get(api_serve_file))
        .route("/api/generated/{filename}", get(api_serve_generated))
        .route("/api/generate/{cid}", post(api_generate))
        .route("/api/chat/stream/{cid}/{mid}", get(api_stream))
        .route("/api/log", post(api_log))
        .route("/api/whatsapp/status", get(api_whatsapp_status))
        .route("/api/whatsapp/start", post(api_whatsapp_start))
        .route("/api/whatsapp/stop", post(api_whatsapp_stop))
        .route("/api/whatsapp/qr", get(api_whatsapp_qr))
        .route("/api/whatsapp/inbound", post(api_whatsapp_inbound))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    info!("Gnomef Rust backend listening on http://{bind_addr}");
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            let cfg = shutdown_state.config.read().await.clone();
            shutdown_state.whatsapp.stop(&cfg).await;
        })
        .await?;
    Ok(())
}

async fn index(State(state): State<AppState>) -> Result<Html<String>, AppError> {
    let html = if state.paths.index_file.exists() {
        fs::read_to_string(&state.paths.index_file)?
    } else {
        "<h1>Gnomef Rust backend</h1><p>Place index.html next to the project or set GNOMEF_RS_HOME.</p>".into()
    };
    Ok(Html(html))
}

async fn api_get_config(State(state): State<AppState>) -> Json<AppConfig> {
    Json(state.config.read().await.clone())
}

async fn api_set_config(
    State(state): State<AppState>,
    Json(patch): Json<Value>,
) -> Result<Json<Value>, AppError> {
    let cfg = {
        let mut cfg = state.config.write().await;
        cfg.merge_patch(&patch);
        cfg.save(&state.paths.config_file)?;
        cfg.clone()
    };
    if cfg.whatsapp_enabled {
        state.whatsapp.start(&cfg, &state.paths, true).await?;
    } else {
        state.whatsapp.stop(&cfg).await;
    }
    Ok(Json(json!({"ok": true})))
}

async fn api_models(State(state): State<AppState>) -> Json<Value> {
    let cfg = state.config.read().await.clone();
    let models = state.llama.list_models(&cfg).await.unwrap_or_default();
    if models.is_empty() {
        Json(json!([cfg.default_model]))
    } else {
        Json(json!(models.into_iter().map(|m| m.id).collect::<Vec<_>>()))
    }
}

async fn api_get_memory(State(state): State<AppState>) -> Json<Value> {
    let memory = state.memory.read().await;
    Json(serde_json::to_value(&*memory).unwrap_or_else(|_| json!({})))
}

async fn api_runtime_profile(State(state): State<AppState>) -> Json<Value> {
    Json(state.runtime_profile.as_json())
}

async fn api_list_chats(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    Ok(Json(json!(list_chats(&state.paths)?)))
}

async fn api_new_chat(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    Ok(Json(json!(new_chat(&state.paths)?)))
}

async fn api_get_chat(
    State(state): State<AppState>,
    AxumPath(cid): AxumPath<String>,
) -> Result<Json<Value>, AppError> {
    Ok(Json(json!(load_chat(&state.paths, &cid)?)))
}

async fn api_delete_chat(
    State(state): State<AppState>,
    AxumPath(cid): AxumPath<String>,
) -> Result<Json<Value>, AppError> {
    delete_chat(&state.paths, &cid)?;
    let active = new_chat(&state.paths)?.id;
    Ok(Json(json!({"ok": true, "active": active})))
}

async fn api_clear_chat(
    State(state): State<AppState>,
    AxumPath(cid): AxumPath<String>,
) -> Result<Json<Value>, AppError> {
    clear_chat(&state.paths, &cid)?;
    Ok(Json(json!({"ok": true})))
}

async fn api_add_message(
    State(state): State<AppState>,
    AxumPath(cid): AxumPath<String>,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, AppError> {
    let role = payload
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("user");
    let content = payload
        .get("content")
        .cloned()
        .unwrap_or(Value::String(String::new()));
    let cfg = state.config.read().await.clone();
    append_msg(&state.paths, &cfg, &cid, role, content, Map::new())?;
    let chat = load_chat(&state.paths, &cid)?;
    update_recent_memory_snapshot(&state.memory, &state.paths, &cfg, &chat).await?;
    if is_assistant_role(role) {
        spawn_memory_refresh(state.clone(), chat.clone(), cfg.default_model.clone());
    }
    Ok(Json(json!({"ok": true, "count": chat.messages.len()})))
}

async fn api_start_stream(
    State(state): State<AppState>,
    AxumPath(cid): AxumPath<String>,
) -> Result<Json<Value>, AppError> {
    let chat = load_chat(&state.paths, &cid)?;
    Ok(Json(json!({"ok": true, "index": chat.messages.len()})))
}

async fn api_get_pending_question(
    State(state): State<AppState>,
    AxumPath(cid): AxumPath<String>,
) -> Json<Value> {
    Json(state.pending_questions.public_view(&cid).await)
}

async fn api_answer_pending_question(
    State(state): State<AppState>,
    AxumPath(cid): AxumPath<String>,
    Json(payload): Json<Value>,
) -> Response {
    match state.pending_questions.answer(&cid, &payload).await {
        Ok(value) => Json(value).into_response(),
        Err(err) => (
            err.status_code(),
            Json(json!({"ok": false, "error": err.to_string()})),
        )
            .into_response(),
    }
}

async fn api_chat_tasks(
    State(state): State<AppState>,
    AxumPath(cid): AxumPath<String>,
) -> Result<Json<Value>, AppError> {
    Ok(Json(workflow_tasks::task_list(&state.paths, &cid)?))
}

async fn api_stop_task(
    State(state): State<AppState>,
    AxumPath(task_id): AxumPath<String>,
    payload: Option<Json<Value>>,
) -> Result<Json<Value>, AppError> {
    let scope = payload
        .as_ref()
        .and_then(|Json(value)| value.get("scope"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .unwrap_or("default");
    let mut response = workflow_tasks::task_stop(&state.paths, scope, &task_id)?;
    let runtime_stopped = state.runtime.stop(&task_id).await;
    if let Some(obj) = response.as_object_mut() {
        obj.insert("runtime_stopped".into(), json!(runtime_stopped));
    }
    Ok(Json(response))
}

async fn api_runtime_task_update(
    State(state): State<AppState>,
    AxumPath(task_id): AxumPath<String>,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, AppError> {
    Ok(Json(workflow_tasks::runtime_task_update(
        &state.paths,
        &task_id,
        &payload,
    )?))
}

async fn api_upload(
    State(state): State<AppState>,
    AxumPath(cid): AxumPath<String>,
    mut multipart: Multipart,
) -> Result<Json<Value>, AppError> {
    let Some(field) = multipart.next_field().await? else {
        return Ok(Json(json!({"ok": false, "error": "No file provided"})));
    };
    let original = field.file_name().unwrap_or("upload.bin").to_string();
    let filename = sanitize_upload_filename(&original, "upload.bin");
    let data = field.bytes().await?;
    if data.len() > 50 * 1024 * 1024 {
        return Ok(Json(
            json!({"ok": false, "error": "File too large (max 50 MB)"}),
        ));
    }

    let dest = unique_upload_path(&state.paths.uploads_dir, &filename)?;
    tokio::fs::write(&dest, &data).await?;
    let read = read_file(&dest)?;
    let extracted = if read.content.is_empty() {
        read.ocr.clone()
    } else {
        read.content.clone()
    };

    let cfg = state.config.read().await.clone();
    let final_name = dest
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&filename)
        .to_string();
    let meta = if read.file_type == "image" {
        image_meta(&final_name, &dest, &extracted)
    } else {
        json!({"type": read.file_type, "filename": final_name, "path": dest.to_string_lossy()})
    };
    append_msg(&state.paths, &cfg, &cid, "user", meta, Map::new())?;
    if !extracted.trim().is_empty() {
        append_msg(
            &state.paths,
            &cfg,
            &cid,
            "user",
            Value::String(format!(
                "[Extracted content from uploaded file: {final_name}]\n\n{}",
                extracted.chars().take(20_000).collect::<String>()
            )),
            Map::new(),
        )?;
    }
    let chat = load_chat(&state.paths, &cid)?;
    update_recent_memory_snapshot(&state.memory, &state.paths, &cfg, &chat).await?;

    Ok(Json(
        json!({"ok": true, "type": read.file_type, "filename": final_name}),
    ))
}

async fn api_serve_file(
    State(state): State<AppState>,
    AxumPath(filename): AxumPath<String>,
) -> Result<Response, AppError> {
    let path = state
        .paths
        .uploads_dir
        .join(sanitize_upload_filename(&filename, "file"));
    if !path.exists() {
        return Ok((StatusCode::NOT_FOUND, "File not found").into_response());
    }
    Ok(tokio::fs::read(path).await?.into_response())
}

async fn api_serve_generated(
    State(state): State<AppState>,
    AxumPath(filename): AxumPath<String>,
) -> Result<Response, AppError> {
    let path = state
        .paths
        .generated_dir
        .join(sanitize_upload_filename(&filename, "file"));
    if !path.exists() {
        return Ok((StatusCode::NOT_FOUND, "File not found").into_response());
    }
    Ok(tokio::fs::read(path).await?.into_response())
}

#[derive(Debug, Deserialize)]
struct GeneratePayload {
    query: String,
    model: Option<String>,
}

async fn api_generate(
    State(state): State<AppState>,
    AxumPath(cid): AxumPath<String>,
    Json(payload): Json<GeneratePayload>,
) -> Result<Response, AppError> {
    let Some(output_format) = detect_format(&payload.query) else {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Could not detect output format"})),
        )
            .into_response());
    };

    let cfg = state.config.read().await.clone();
    let chat = load_chat(&state.paths, &cid)?;
    let (model, _) = resolve_model(&state.llama, &cfg, payload.model.as_deref()).await;
    match generate_document(
        &state.llama,
        &cfg,
        &state.paths,
        Some(state.memory.clone()),
        &model,
        &payload.query,
        &chat.messages,
        Some(&cid),
        output_format,
    )
    .await
    {
        Ok(response) => Ok(Json::<Value>(response).into_response()),
        Err(err) => Ok((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("Model error: {err}")})),
        )
            .into_response()),
    }
}

#[derive(Debug, Deserialize)]
struct StreamQuery {
    query: String,
    model: Option<String>,
}

async fn api_stream(
    State(state): State<AppState>,
    AxumPath((cid, _mid)): AxumPath<(String, String)>,
    Query(params): Query<StreamQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let state_clone = state.clone();
    Sse::new(stream! {
        let stream_client = state_clone.llama.clone();
        let result = async move {
            let cfg = state_clone.config.read().await.clone();
            let chat = load_chat(&state_clone.paths, &cid)?;
            let (model, known_models) = resolve_model(
                &state_clone.llama,
                &cfg,
                params.model.as_deref(),
            ).await;
            let plan = prepare_streaming_response_with_uploads(
                &state_clone.llama,
                &cfg,
                &state_clone.paths,
                Some(state_clone.memory.clone()),
                &state_clone.runtime_profile,
                &model,
                &known_models,
                &params.query,
                &chat.messages,
                Some(&cid),
                &state_clone.pending_questions,
                &state_clone.runtime,
                Some(state_clone.config.clone()),
            ).await;
            Ok::<_, anyhow::Error>((cfg, model, plan))
        }.await;

        match result {
            Ok((cfg, model, StreamResponsePlan::Direct { messages, temperature })) => {
                match stream_client.chat_stream(&cfg, &model, messages, temperature).await {
                    Ok(mut tokens) => {
                        while let Some(token) = tokens.next().await {
                            match token {
                                Ok(token) if !token.is_empty() => {
                                    yield Ok(Event::default().data(json!({"token": token}).to_string()));
                                }
                                Ok(_) => {}
                                Err(err) => {
                                    yield Ok(Event::default().data(json!({"token": format!("[Stream error: {err}]")}).to_string()));
                                    break;
                                }
                            }
                        }
                    }
                    Err(err) => {
                        yield Ok(Event::default().data(json!({"token": format!("[Stream error: {err}]")}).to_string()));
                    }
                }
            }
            Ok((_cfg, _model, StreamResponsePlan::Fallback(reply))) => {
                for chunk in chunk_text_for_sse(&reply, 120) {
                    yield Ok(Event::default().data(json!({"token": chunk}).to_string()));
                }
            }
            Err(err) => {
                yield Ok(Event::default().data(json!({"token": format!("[Error: {err}]")}).to_string()));
            }
        }
        yield Ok(Event::default().data("[DONE]"));
    })
}

async fn api_log(Json(payload): Json<Value>) -> Json<Value> {
    let level = payload
        .get("level")
        .and_then(Value::as_str)
        .unwrap_or("info");
    let msg = payload.get("msg").and_then(Value::as_str).unwrap_or("");
    match level {
        "error" => error!("[FE] {msg}"),
        "warn" | "warning" => warn!("[FE] {msg}"),
        _ => info!("[FE] {msg}"),
    }
    Json(json!({"ok": true}))
}

async fn api_whatsapp_status(State(state): State<AppState>) -> Json<Value> {
    let cfg = state.config.read().await.clone();
    Json(state.whatsapp.status(&cfg, &state.paths).await)
}

async fn api_whatsapp_start(State(state): State<AppState>) -> Result<Response, AppError> {
    let cfg = state.config.read().await.clone();
    match state.whatsapp.start(&cfg, &state.paths, false).await {
        Ok(message) => Ok(Json(json!({"ok": true, "message": message})).into_response()),
        Err(err) => Ok((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok": false, "error": err.to_string()})),
        )
            .into_response()),
    }
}

async fn api_whatsapp_stop(State(state): State<AppState>) -> Json<Value> {
    let cfg = state.config.read().await.clone();
    state.whatsapp.stop(&cfg).await;
    Json(json!({"ok": true}))
}

async fn api_whatsapp_qr(State(state): State<AppState>) -> Result<Response, AppError> {
    let cfg = state.config.read().await.clone();
    let status = state.whatsapp.status(&cfg, &state.paths).await;
    let qr_data = status.get("qr").and_then(Value::as_str).unwrap_or("");
    if qr_data.is_empty() {
        return Ok((StatusCode::NOT_FOUND, "No WhatsApp QR available").into_response());
    }
    let svg = qr_svg(qr_data)?;
    Ok((
        StatusCode::OK,
        [(CONTENT_TYPE, "image/svg+xml; charset=utf-8")],
        svg,
    )
        .into_response())
}

async fn api_whatsapp_inbound(
    State(state): State<AppState>,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, AppError> {
    let message_id = payload
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if !message_id.is_empty() {
        let mut seen = state.wa_seen.lock().await;
        if seen.contains(&message_id) {
            return Ok(Json(json!({"ok": true, "duplicate": true})));
        }
        seen.insert(message_id);
    }

    let chat_jid = payload
        .get("chat_jid")
        .and_then(Value::as_str)
        .unwrap_or("");
    if chat_jid.is_empty() {
        return Ok(Json(json!({"ok": false, "error": "chat_jid is required"})));
    }
    let cfg = state.config.read().await.clone();
    if !should_process_whatsapp_message(&cfg, &state.paths, &payload) {
        return Ok(Json(json!({"ok": true, "skipped": true})));
    }

    let cid = whatsapp_chat_id(chat_jid);
    let chat_name = payload
        .get("chat_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let sender_name = payload
        .get("sender_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    ensure_whatsapp_chat(&state, &cid, chat_jid, chat_name, sender_name)?;

    let mut content = payload
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let extra = whatsapp_extra(&payload, chat_jid);

    if let Some(media) = payload.get("media").and_then(Value::as_object) {
        if media.get("type").and_then(Value::as_str) == Some("image") {
            let b64 = media
                .get("data_base64")
                .or_else(|| media.get("data"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if !b64.is_empty() {
                let filename = media.get("filename").and_then(Value::as_str);
                let mimetype = media.get("mimetype").and_then(Value::as_str);
                let (final_name, path, read) = save_base64_image(
                    &state.paths,
                    &state.paths.whatsapp_media_dir,
                    filename,
                    mimetype,
                    b64,
                )?;
                let extracted = if read.content.is_empty() {
                    read.ocr
                } else {
                    read.content
                };
                append_msg(
                    &state.paths,
                    &cfg,
                    &cid,
                    "user",
                    image_meta(&final_name, &path, &extracted),
                    extra.clone(),
                )?;
                if !extracted.trim().is_empty() {
                    append_msg(
                        &state.paths,
                        &cfg,
                        &cid,
                        "user",
                        Value::String(format!(
                            "[Extracted content from uploaded file: {final_name}]\n\n{}",
                            extracted.chars().take(20_000).collect::<String>()
                        )),
                        extra.clone(),
                    )?;
                }
                if content.is_empty() {
                    content = format!("Analizeaza imaginea atasata: {final_name}");
                }
            }
        }
    }
    if content.is_empty() {
        content = "Am primit un atasament WhatsApp pe care nu il pot procesa inca.".into();
    }

    append_msg(
        &state.paths,
        &cfg,
        &cid,
        "user",
        Value::String(content.clone()),
        extra,
    )?;
    let chat = load_chat(&state.paths, &cid)?;
    let (model, known_models) = resolve_model(&state.llama, &cfg, None).await;
    let reply = generate_chat_response_with_uploads(
        &state.llama,
        &cfg,
        &state.paths,
        Some(state.memory.clone()),
        &state.runtime_profile,
        &model,
        &known_models,
        &content,
        &chat.messages,
        Some(&cid),
        &state.pending_questions,
        &state.runtime,
        Some(state.config.clone()),
    )
    .await;
    append_msg(
        &state.paths,
        &cfg,
        &cid,
        "gnome",
        Value::String(reply.clone()),
        Map::from_iter([
            ("channel".into(), json!("whatsapp")),
            ("chat_jid".into(), json!(chat_jid)),
        ]),
    )?;
    let updated_chat = load_chat(&state.paths, &cid)?;
    update_recent_memory_snapshot(&state.memory, &state.paths, &cfg, &updated_chat).await?;
    spawn_memory_refresh(state.clone(), updated_chat, model);
    send_whatsapp_message(&cfg, chat_jid, &reply).await;
    Ok(Json(json!({"ok": true})))
}

fn whatsapp_chat_id(chat_jid: &str) -> String {
    let safe = chat_jid
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .to_string();
    format!("wa_{}", if safe.is_empty() { "chat" } else { &safe })
}

fn ensure_whatsapp_chat(
    state: &AppState,
    cid: &str,
    chat_jid: &str,
    chat_name: &str,
    sender_name: &str,
) -> anyhow::Result<()> {
    let mut chat = load_chat(&state.paths, cid)?;
    let title = compact_ws(chat_name)
        .or_else(|| compact_ws(sender_name))
        .unwrap_or_else(|| chat_jid.to_string());
    let pretty_title = if title.starts_with("WhatsApp") {
        title
    } else {
        format!("WhatsApp - {title}")
    };
    if chat.title == *cid || chat.title.trim().is_empty() {
        chat.title = pretty_title;
    }
    chat.extra.insert("channel".into(), json!("whatsapp"));
    chat.extra.insert("jid".into(), json!(chat_jid));
    save_chat(&state.paths, &chat)
}

fn should_process_whatsapp_message(cfg: &AppConfig, paths: &AppPaths, payload: &Value) -> bool {
    let content = payload
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let chat_jid = payload
        .get("chat_jid")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let has_media = payload.get("media").and_then(Value::as_object).is_some();
    if (content.is_empty() && !has_media) || chat_jid.is_empty() {
        return false;
    }
    if payload
        .get("is_bot_message")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return false;
    }

    if !cfg.whatsapp_allowed_jids.is_empty() {
        return cfg
            .whatsapp_allowed_jids
            .iter()
            .any(|allowed| allowed.trim() == chat_jid);
    }

    let own_jid = payload
        .get("own_jid")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| self_whatsapp_jid(paths));
    !own_jid.is_empty() && own_jid == chat_jid
}

fn compact_ws(text: &str) -> Option<String> {
    let item = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!item.is_empty()).then_some(item)
}

fn whatsapp_extra(payload: &Value, chat_jid: &str) -> Map<String, Value> {
    let mut extra = Map::new();
    extra.insert("channel".into(), json!("whatsapp"));
    extra.insert("chat_jid".into(), json!(chat_jid));
    for key in ["sender", "sender_name", "is_self_chat"] {
        if let Some(value) = payload.get(key) {
            extra.insert(key.into(), value.clone());
        }
    }
    extra
}

fn is_assistant_role(role: &str) -> bool {
    matches!(role.trim().to_lowercase().as_str(), "assistant" | "gnome")
}

fn spawn_memory_refresh(state: AppState, chat: crate::storage::Chat, model: String) {
    tokio::spawn(async move {
        let cfg = state.config.read().await.clone();
        if let Err(err) = refresh_memory_from_chat(
            &state.llama,
            &cfg,
            &state.paths,
            &state.memory,
            &model,
            &chat,
        )
        .await
        {
            warn!("Memory refresh failed for {}: {err}", chat.id);
        }
    });
}
