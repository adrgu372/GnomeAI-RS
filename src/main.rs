mod app_dirs;
mod chat_logic;
mod codex_app_server;
mod config;
mod consistency;
mod firecrawl;
mod generation;
mod llama;
mod memory;
mod provider;
mod provider_catalog;
mod questions;
mod runtime;
mod runtime_profile;
mod sandbox;
mod search;
mod storage;
mod tasks;
mod tools;
mod uploads;
mod vision;
mod whatsapp;

use std::{
    collections::{HashSet, VecDeque},
    convert::Infallible,
    fs,
    net::SocketAddr,
    os::unix::fs::PermissionsExt,
    sync::Arc,
};

use async_stream::stream;
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Multipart, Path as AxumPath, Query, Request, State},
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{CONTENT_DISPOSITION, CONTENT_TYPE, HOST},
    },
    middleware,
    middleware::Next,
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
use tower_http::trace::TraceLayer;
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
    provider_catalog::{
        AuthKind, PROVIDERS, ProviderSelection, ProviderSettingsStore, WireProtocol, preset,
    },
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
    wa_seen: Arc<Mutex<SeenMessages>>,
    api_token: Arc<str>,
}

const WA_SEEN_LIMIT: usize = 10_000;

#[derive(Default)]
struct SeenMessages {
    ids: HashSet<String>,
    order: VecDeque<String>,
}

impl SeenMessages {
    /// Returns true for a duplicate. New IDs are retained in bounded FIFO
    /// order so a long-lived bridge cannot grow memory without limit.
    fn remember(&mut self, id: String) -> bool {
        if self.ids.contains(&id) {
            return true;
        }
        self.ids.insert(id.clone());
        self.order.push_back(id);
        while self.order.len() > WA_SEEN_LIMIT {
            if let Some(expired) = self.order.pop_front() {
                self.ids.remove(&expired);
            }
        }
        false
    }
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

fn main() -> anyhow::Result<()> {
    // The sandbox helper must run before Tokio starts worker threads.
    sandbox::maybe_run_as_helper()?;
    web_main()
}

#[tokio::main]
async fn web_main() -> anyhow::Result<()> {
    let startup_trace = std::env::var_os("GNOMEF_STARTUP_TRACE").is_some();
    let trace_startup = |stage: &str| {
        if startup_trace {
            eprintln!("gnomef-web startup: {stage}");
        }
    };
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("gnomef_rs=info".parse()?))
        .init();
    trace_startup("logging ready");

    let launch_dir = std::env::current_dir()?;
    let app_dir = app_dirs::resolve_app_home(&launch_dir)?;
    let paths = Arc::new(AppPaths::new(app_dir)?);
    trace_startup("state paths ready");
    let memory = Arc::new(RwLock::new(MemoryStore::load(paths.as_ref())?));
    let runtime_profile = Arc::new(RuntimeProfile::detect(paths.as_ref()));
    trace_startup("memory and runtime profile ready");
    let mut config = AppConfig::load(&paths.config_file)?;
    let configured_api_token = std::env::var("GNOMEF_WEB_TOKEN")
        .ok()
        .filter(|token| token.len() >= 16);
    let has_configured_api_token = configured_api_token.is_some();
    let api_token = configured_api_token.unwrap_or_else(|| {
        format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        )
    });
    config.web_api_token = api_token.clone();
    let provider_settings = ProviderSettingsStore::new(paths.store_dir.join("providers.json"));
    if let Some(selection) = provider_settings.load()? {
        apply_provider_selection(&selection, &mut config);
    }
    trace_startup("configuration ready");

    let bind_addr: SocketAddr = format!("{}:{}", config.host, config.port).parse()?;
    let allow_remote = std::env::var("GNOMEF_ALLOW_REMOTE").as_deref() == Ok("1");
    if !bind_addr.ip().is_loopback() {
        if !allow_remote || !has_configured_api_token {
            anyhow::bail!(
                "refusing to expose WebTool on non-loopback address {bind_addr}; \
                 set GNOMEF_ALLOW_REMOTE=1 and a GNOMEF_WEB_TOKEN of at least 16 characters"
            );
        }
    }
    let state = AppState {
        paths,
        config: Arc::new(RwLock::new(config)),
        memory,
        runtime_profile,
        llama: LlamaClient::new(),
        pending_questions: PendingQuestions::default(),
        runtime: RuntimeHandles::default(),
        whatsapp: WhatsAppBridge::new(),
        wa_seen: Arc::new(Mutex::new(SeenMessages::default())),
        api_token: Arc::from(api_token),
    };
    trace_startup("application state ready");

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
        .route("/api/providers", get(api_get_providers))
        .route("/api/provider", post(api_set_provider))
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
        .route("/api/chats/{cid}/rename", post(api_rename_chat))
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
        .layer(DefaultBodyLimit::max(51 * 1024 * 1024))
        .layer(TraceLayer::new_for_http())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            protect_local_api,
        ))
        .with_state(state);
    trace_startup("router ready");

    info!("Gnomef Rust backend listening on http://{bind_addr}");
    let listener = tokio::net::TcpListener::bind(bind_addr).await?;
    trace_startup("listener bound");
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            let cfg = shutdown_state.config.read().await.clone();
            shutdown_state.whatsapp.stop(&cfg).await;
        })
        .await?;
    Ok(())
}

async fn index(State(state): State<AppState>) -> Result<Response, AppError> {
    let mut html = if state.paths.index_file.exists() {
        fs::read_to_string(&state.paths.index_file)?
    } else {
        "<h1>Gnomef Rust backend</h1><p>Place index.html next to the project or set GNOMEF_RS_HOME.</p>".into()
    };
    let bootstrap = format!(
        "<script>window.__GNOMEF_TOKEN__={};</script>",
        serde_json::to_string(state.api_token.as_ref())?
    );
    if let Some(position) = html.find("<head>") {
        html.insert_str(position + "<head>".len(), &bootstrap);
    } else {
        html.insert_str(0, &bootstrap);
    }
    let mut headers = HeaderMap::new();
    headers.insert(
        "content-security-policy",
        HeaderValue::from_static(
            "default-src 'self'; script-src 'self' 'unsafe-inline'; \
             style-src 'self' 'unsafe-inline'; img-src 'self' data:; \
             connect-src 'self'; frame-ancestors 'none'; base-uri 'none'",
        ),
    );
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    headers.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    Ok((headers, Html(html)).into_response())
}

async fn protect_local_api(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let allow_remote = std::env::var("GNOMEF_ALLOW_REMOTE").as_deref() == Ok("1");
    let host_allowed = request
        .headers()
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|host| allow_remote || is_loopback_host(host));
    if !host_allowed {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"ok": false, "error": "invalid Host header"})),
        )
            .into_response();
    }

    if request.uri().path() != "/" {
        let supplied = request
            .headers()
            .get("x-gnomef-token")
            .and_then(|value| value.to_str().ok())
            .or_else(|| query_value(request.uri().query(), "token"));
        if !supplied.is_some_and(|token| constant_time_eq(token, state.api_token.as_ref())) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"ok": false, "error": "missing or invalid WebTool token"})),
            )
                .into_response();
        }
        // Query tokens are necessary for EventSource and direct downloads,
        // which cannot attach our custom header. Remove the secret before the
        // trace layer and route extractors see the URI.
        strip_query_key(&mut request, "token");
    }
    next.run(request).await
}

fn is_loopback_host(host: &str) -> bool {
    let host = host.trim().to_ascii_lowercase();
    host == "localhost"
        || host.starts_with("localhost:")
        || host == "127.0.0.1"
        || host.starts_with("127.0.0.1:")
        || host == "[::1]"
        || host.starts_with("[::1]:")
}

fn query_value<'a>(query: Option<&'a str>, key: &str) -> Option<&'a str> {
    query?.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == key).then_some(value)
    })
}

fn strip_query_key(request: &mut Request, key: &str) {
    let Some(query) = request.uri().query() else {
        return;
    };
    let filtered = query
        .split('&')
        .filter(|pair| pair.split_once('=').is_none_or(|(name, _)| name != key))
        .collect::<Vec<_>>()
        .join("&");
    let replacement = if filtered.is_empty() {
        request.uri().path().to_string()
    } else {
        format!("{}?{filtered}", request.uri().path())
    };
    if let Ok(uri) = replacement.parse() {
        *request.uri_mut() = uri;
    }
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.as_bytes()
        .iter()
        .zip(right.as_bytes())
        .fold(0u8, |difference, (left, right)| difference | (left ^ right))
        == 0
}

async fn api_get_config(State(state): State<AppState>) -> Json<Value> {
    let config = state.config.read().await.clone();
    let mut value = serde_json::to_value(&config).unwrap_or_else(|_| json!({}));
    value["llama_api_key"] = json!("");
    value["firecrawl_api_key"] = json!("");
    value["has_provider_api_key"] = json!(!config.llama_api_key.trim().is_empty());
    value["has_firecrawl_api_key"] = json!(!config.firecrawl_api_key.trim().is_empty());
    Json(value)
}

async fn api_get_providers(State(state): State<AppState>) -> Json<Value> {
    let config = state.config.read().await.clone();
    let providers = PROVIDERS
        .iter()
        .map(|provider| {
            json!({
                "id": provider.id,
                "name": provider.name,
                "auth": match provider.auth {
                    AuthKind::ApiKey => "api_key",
                    AuthKind::Account => "account",
                    AuthKind::OptionalApiKey => "optional_api_key",
                },
                "protocol": match provider.protocol {
                    WireProtocol::OpenAi => "openai",
                    WireProtocol::Anthropic => "anthropic",
                    WireProtocol::CodexAppServer => "codex",
                    WireProtocol::ClaudeCli => "claude-cli",
                },
                "base_url": provider.base_url,
                "default_model": provider.default_model,
                "description": provider.description,
                "web_supported": provider.auth != AuthKind::Account,
            })
        })
        .collect::<Vec<_>>();
    Json(json!({
        "providers": providers,
        "current": {
            "provider_id": config.provider_id,
            "model": config.default_model,
            "base_url": config.llama_base_url,
            "has_api_key": !config.llama_api_key.trim().is_empty(),
            "web_search_enabled": config.web_search_enabled,
            "memory_enabled": config.memory_enabled,
            "memory_max_age_days": config.memory_max_age_days,
        }
    }))
}

async fn api_set_provider(
    State(state): State<AppState>,
    Json(payload): Json<Value>,
) -> Result<Response, AppError> {
    let provider_id = payload
        .get("provider_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let Some(provider) = preset(provider_id) else {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": "Unknown provider"})),
        )
            .into_response());
    };
    if provider.auth == AuthKind::Account {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({
                "ok": false,
                "error": "Account login is available in the terminal agent. WebTool supports API providers."
            })),
        )
            .into_response());
    }

    let settings = ProviderSettingsStore::new(state.paths.store_dir.join("providers.json"));
    let existing = settings.load()?;
    let supplied_key = payload
        .get("api_key")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let api_key = supplied_key.or_else(|| {
        existing
            .as_ref()
            .filter(|selection| selection.provider_id == provider_id)
            .and_then(|selection| selection.api_key().map(str::to_string))
    });
    let base_url = payload
        .get("base_url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let mut selection =
        match ProviderSelection::from_choice(provider_id.to_string(), api_key, base_url) {
            Ok(selection) => selection,
            Err(error) => {
                return Ok((
                    StatusCode::BAD_REQUEST,
                    Json(json!({"ok": false, "error": error.to_string()})),
                )
                    .into_response());
            }
        };
    if let Some(model) = payload
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        selection.model = model.to_string();
    }
    settings.save(&selection)?;
    {
        let mut config = state.config.write().await;
        apply_provider_selection(&selection, &mut config);
        if let Some(enabled) = payload.get("web_search_enabled").and_then(Value::as_bool) {
            config.web_search_enabled = enabled;
        }
        if let Some(enabled) = payload.get("memory_enabled").and_then(Value::as_bool) {
            config.memory_enabled = enabled;
        }
        if let Some(days) = payload.get("memory_max_age_days").and_then(Value::as_u64) {
            config.memory_max_age_days = u32::try_from(days).unwrap_or(3_650);
            config.normalize();
        }
        config.save(&state.paths.config_file)?;
    }
    Ok(Json(json!({
        "ok": true,
        "provider_id": selection.provider_id,
        "model": selection.model,
    }))
    .into_response())
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

async fn api_rename_chat(
    State(state): State<AppState>,
    AxumPath(cid): AxumPath<String>,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, AppError> {
    let title = payload
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .chars()
        .take(80)
        .collect::<String>();
    if title.is_empty() {
        return Ok(Json(json!({"ok": false, "error": "title required"})));
    }
    let mut chat = load_chat(&state.paths, &cid)?;
    chat.title = title;
    save_chat(&state.paths, &chat)?;
    Ok(Json(json!({"ok": true, "title": chat.title})))
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
    tokio::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o600)).await?;
    let read = read_file(&dest).await?;
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
    serve_download(path, false).await
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
    serve_download(path, true).await
}

async fn serve_download(
    path: std::path::PathBuf,
    force_attachment: bool,
) -> Result<Response, AppError> {
    let bytes = tokio::fs::read(&path).await?;
    let mime = mime_guess::from_path(&path).first_or_octet_stream();
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("download");
    let safe_inline_image = matches!(
        path.extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp"
    );
    let disposition = if !force_attachment && safe_inline_image {
        format!("inline; filename=\"{filename}\"")
    } else {
        format!("attachment; filename=\"{filename}\"")
    };
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_str(mime.as_ref())?);
    headers.insert(CONTENT_DISPOSITION, HeaderValue::from_str(&disposition)?);
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    Ok((headers, bytes).into_response())
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
        if seen.remember(message_id) {
            return Ok(Json(json!({"ok": true, "duplicate": true})));
        }
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
                )
                .await?;
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

    // Never trust identity fields supplied by the inbound payload. The only
    // authoritative self JID is the credential state written by Baileys.
    let own_jid = self_whatsapp_jid(paths);
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

fn apply_provider_selection(selection: &ProviderSelection, config: &mut AppConfig) {
    config.provider_id = selection.provider_id.clone();
    config.provider_protocol = selection.protocol_name().to_string();
    config.default_model = selection.model.clone();
    config.llama_api_key = selection.api_key().unwrap_or_default().to_string();
    if let Some(base_url) = selection.resolved_base_url() {
        config.llama_base_url = base_url.to_string();
    }
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

#[cfg(test)]
mod web_security_tests {
    use super::*;

    #[test]
    fn only_loopback_host_headers_are_accepted_by_default() {
        assert!(is_loopback_host("127.0.0.1:8787"));
        assert!(is_loopback_host("localhost:8787"));
        assert!(is_loopback_host("[::1]:8787"));
        assert!(!is_loopback_host("attacker.example:8787"));
        assert!(!is_loopback_host("127.0.0.1.attacker.example"));
    }

    #[test]
    fn api_token_helpers_match_exact_values() {
        assert_eq!(
            query_value(Some("query=hello&token=secret&model=x"), "token"),
            Some("secret")
        );
        assert!(constant_time_eq("same-token", "same-token"));
        assert!(!constant_time_eq("same-token", "other-token"));
        assert!(!constant_time_eq("short", "longer"));

        let mut request = Request::builder()
            .uri("/api/chat/stream/c/m?query=hello&token=secret&model=x")
            .body(axum::body::Body::empty())
            .unwrap();
        strip_query_key(&mut request, "token");
        assert_eq!(
            request.uri().path_and_query().unwrap().as_str(),
            "/api/chat/stream/c/m?query=hello&model=x"
        );
    }

    #[test]
    fn whatsapp_deduplication_cache_is_bounded() {
        let mut seen = SeenMessages::default();
        assert!(!seen.remember("first".into()));
        assert!(seen.remember("first".into()));
        for index in 0..=WA_SEEN_LIMIT {
            assert!(!seen.remember(format!("id-{index}")));
        }
        assert!(seen.ids.len() <= WA_SEEN_LIMIT);
        assert!(seen.order.len() <= WA_SEEN_LIMIT);
        assert!(!seen.remember("first".into()));
    }
}
