mod app_dirs;
mod chat_logic;
mod codex_app_server;
mod config;
mod consistency;
mod embeddings;
mod firecrawl;
mod generation;
mod llama;
mod memory;
mod memory_engine;
mod openrouter;
mod privilege;
mod protocol;
mod provider;
mod provider_catalog;
mod questions;
mod runtime;
mod runtime_profile;
mod sandbox;
mod search;
mod skills;
mod storage;
mod tasks;
mod tools;
mod transcribe;
mod turn_stream;
mod uploads;
mod vision;
mod web_approvals;
mod whatsapp;
mod workspaces;

use std::{
    collections::{HashSet, VecDeque},
    convert::Infallible,
    fs,
    io::Read,
    net::SocketAddr,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
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
        StreamResponsePlan, generate_chat_response_with_uploads,
        prepare_streaming_response_with_uploads, resolve_model,
    },
    config::AppConfig,
    generation::{detect_format, generate_document},
    llama::LlamaClient,
    memory_engine::{DreamHandle, MemoryEngine, spawn_dream_worker},
    protocol::Event as TurnEvent,
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
    turn_stream::{LiveTurns, TurnStream, WebEvent},
    uploads::{
        ensure_upload_capacity, read_file, sanitize_upload_filename, save_base64_media,
        transcribe_media, write_unique_private,
    },
    web_approvals::{ApprovalAnswerPayload, PendingApprovals},
    whatsapp::{WhatsAppBridge, qr_svg, self_whatsapp_jid, send_whatsapp_message},
    workspaces::{WorkspaceHistory, resolve_startup_workspace},
};

#[derive(Clone)]
struct AppState {
    paths: Arc<AppPaths>,
    config: Arc<RwLock<AppConfig>>,
    memory: Arc<MemoryEngine>,
    dream: DreamHandle,
    llama: LlamaClient,
    pending_questions: PendingQuestions,
    pending_approvals: PendingApprovals,
    runtime: RuntimeHandles,
    whatsapp: WhatsAppBridge,
    wa_seen: Arc<Mutex<SeenMessages>>,
    upload_lock: Arc<Mutex<()>>,
    api_token: Arc<str>,
    /// The turn running per conversation, so `/interrupt` can find it.
    turns: LiveTurns,
    workspace: Arc<RwLock<WebWorkspace>>,
    local_web: bool,
    /// Broadcasts complete WhatsApp status snapshots to browser SSE clients.
    whatsapp_events: tokio::sync::broadcast::Sender<Value>,
}

struct WebWorkspace {
    current: PathBuf,
    history: WorkspaceHistory,
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
    let interrupted_agents = workflow_tasks::recover_interrupted_agents(paths.as_ref())?;
    if interrupted_agents > 0 {
        info!("Recovered {interrupted_agents} interrupted subagent task(s)");
    }
    let mut workspace_history = WorkspaceHistory::load(paths.store_dir.join("workspaces.json"));
    let (startup_workspace, workspace_note) =
        resolve_startup_workspace(None, &launch_dir, &workspace_history);
    let startup_workspace = startup_workspace
        .canonicalize()
        .unwrap_or(startup_workspace);
    workspace_history.record(&startup_workspace);
    if let Some(note) = workspace_note {
        info!("Workspace: {note}");
    }
    trace_startup("state paths ready");
    let memory = MemoryEngine::open(paths.as_ref())?;
    trace_startup("memory ready");
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
    let config_state = Arc::new(RwLock::new(config));
    let llama = LlamaClient::new();
    let dream = spawn_dream_worker(memory.clone(), llama.clone(), config_state.clone());
    let state = AppState {
        paths,
        config: config_state,
        memory,
        dream,
        llama,
        pending_questions: PendingQuestions::default(),
        pending_approvals: PendingApprovals::default(),
        runtime: RuntimeHandles::default(),
        whatsapp: WhatsAppBridge::new(),
        wa_seen: Arc::new(Mutex::new(SeenMessages::default())),
        upload_lock: Arc::new(Mutex::new(())),
        api_token: Arc::from(api_token),
        turns: LiveTurns::default(),
        workspace: Arc::new(RwLock::new(WebWorkspace {
            current: startup_workspace,
            history: workspace_history,
        })),
        local_web: bind_addr.ip().is_loopback(),
        whatsapp_events: tokio::sync::broadcast::channel(16).0,
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

    // Poll the local bridge and publish only meaningful status changes. This
    // lives in WebTool because the TUI and WebTool are separate processes.
    {
        let monitor_state = state.clone();
        tokio::spawn(async move {
            let mut previous = None::<Value>;
            loop {
                let cfg = monitor_state.config.read().await.clone();
                let status = monitor_state
                    .whatsapp
                    .status(&cfg, &monitor_state.paths)
                    .await;
                if previous.as_ref() != Some(&status) {
                    let _ = monitor_state.whatsapp_events.send(status.clone());
                    previous = Some(status);
                }
                tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
            }
        });
    }

    let shutdown_state = state.clone();
    let app = Router::new()
        .route("/", get(index))
        .route("/api/config", get(api_get_config).post(api_set_config))
        .route("/api/workspace", get(api_workspace).post(api_set_workspace))
        .route("/api/workspace/browse", get(api_browse_workspace))
        .route("/api/approvals/pending", get(api_pending_approvals))
        .route("/api/approvals/{id}/answer", post(api_answer_approval))
        .route("/api/providers", get(api_get_providers))
        .route("/api/provider", post(api_set_provider))
        .route("/api/memory", get(api_get_memory))
        .route("/api/memory/status", get(api_memory_status))
        .route("/api/memory/dream", post(api_memory_dream))
        .route("/api/memory/reindex", post(api_memory_reindex))
        .route("/api/memory/forget", post(api_memory_forget))
        .route("/api/memory/clear", post(api_memory_clear))
        .route("/api/skills", get(api_skills))
        .route("/api/skills/install", post(api_skill_install))
        .route(
            "/api/skills/{name}",
            get(api_skill_inspect).delete(api_skill_remove),
        )
        .route("/api/skills/{name}/update", post(api_skill_update))
        .route("/api/skills/{name}/verify", get(api_skill_verify))
        .route(
            "/api/skills/{name}/activate/{cid}",
            post(api_skill_activate),
        )
        .route("/api/runtime/profile", get(api_runtime_profile))
        .route("/api/models", get(api_models).post(api_preview_models))
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
        .route("/api/agents", get(api_agents).post(api_launch_agent))
        .route("/api/agents/{agent_id}", get(api_agent))
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
        .route("/api/chats/{cid}/interrupt", post(api_interrupt))
        .route("/api/log", post(api_log))
        .route("/api/whatsapp/status", get(api_whatsapp_status))
        .route("/api/whatsapp/events", get(api_whatsapp_events))
        .route("/api/whatsapp/start", post(api_whatsapp_start))
        .route("/api/whatsapp/stop", post(api_whatsapp_stop))
        .route("/api/whatsapp/qr", get(api_whatsapp_qr))
        .route("/api/whatsapp/qr/refresh", post(api_whatsapp_qr_refresh))
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
            shutdown_state.dream.shutdown();
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
    let loopback_host = request
        .headers()
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .is_some_and(is_loopback_host);
    if !loopback_host && !allow_remote {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"ok": false, "error": "invalid Host header"})),
        )
            .into_response();
    }

    // On loopback, `/` bootstraps the per-process token into the local page.
    // A remotely exposed root must already know the configured token; otherwise
    // serving the bootstrap page would disclose the very credential meant to
    // protect every API route.
    if request.uri().path() != "/" || !loopback_host {
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
    Json(public_config_value(&config))
}

fn public_config_value(config: &AppConfig) -> Value {
    let mut value = serde_json::to_value(&config).unwrap_or_else(|_| json!({}));
    if let Some(object) = value.as_object_mut() {
        object.remove("provider_api_keys");
    }
    value["llama_api_key"] = json!("");
    value["firecrawl_api_key"] = json!("");
    value["has_provider_api_key"] = json!(!config.llama_api_key.trim().is_empty());
    value["has_firecrawl_api_key"] = json!(!config.firecrawl_api_key.trim().is_empty());
    value
}

async fn runtime_paths(state: &AppState) -> AppPaths {
    let workspace = state.workspace.read().await.current.clone();
    let mut paths = state.paths.as_ref().clone();
    paths.workspace_dir = workspace;
    paths
}

async fn current_workspace(state: &AppState) -> PathBuf {
    state.workspace.read().await.current.clone()
}

async fn api_workspace(State(state): State<AppState>) -> Json<Value> {
    let workspace = state.workspace.read().await;
    Json(json!({
        "current": workspace.current,
        "recent": workspace.history.recent(),
        "browse_enabled": state.local_web,
    }))
}

async fn api_set_workspace(
    State(state): State<AppState>,
    Json(payload): Json<Value>,
) -> Result<Response, AppError> {
    if !state.local_web {
        return Ok((
            StatusCode::FORBIDDEN,
            Json(json!({
                "ok": false,
                "error": "Workspace selection is available only on the local WebTool"
            })),
        )
            .into_response());
    }
    let raw = payload
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if raw.is_empty() {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": "path required"})),
        )
            .into_response());
    }
    let path = match PathBuf::from(raw).canonicalize() {
        Ok(path) if path.is_dir() => path,
        Ok(_) => {
            return Ok((
                StatusCode::BAD_REQUEST,
                Json(json!({"ok": false, "error": "Workspace must be a directory"})),
            )
                .into_response());
        }
        Err(error) => {
            return Ok((
                StatusCode::BAD_REQUEST,
                Json(json!({"ok": false, "error": format!("Cannot open workspace: {error}")})),
            )
                .into_response());
        }
    };
    let mut workspace = state.workspace.write().await;
    workspace.current = path.clone();
    workspace.history.record(&path);
    Ok(Json(json!({
        "ok": true,
        "current": path,
        "recent": workspace.history.recent(),
    }))
    .into_response())
}

#[derive(Debug, Deserialize)]
struct WorkspaceBrowseQuery {
    path: Option<String>,
    show_hidden: Option<bool>,
}

async fn api_browse_workspace(
    State(state): State<AppState>,
    Query(query): Query<WorkspaceBrowseQuery>,
) -> Result<Response, AppError> {
    if !state.local_web {
        return Ok((
            StatusCode::FORBIDDEN,
            Json(json!({
                "ok": false,
                "error": "Folder browsing is available only on the local WebTool"
            })),
        )
            .into_response());
    }
    let requested = query
        .path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .unwrap_or(current_workspace(&state).await);
    let directory = match requested.canonicalize() {
        Ok(path) if path.is_dir() => path,
        Ok(_) => {
            return Ok((
                StatusCode::BAD_REQUEST,
                Json(json!({"ok": false, "error": "Path is not a directory"})),
            )
                .into_response());
        }
        Err(error) => {
            return Ok((
                StatusCode::BAD_REQUEST,
                Json(json!({"ok": false, "error": format!("Cannot browse folder: {error}")})),
            )
                .into_response());
        }
    };
    let show_hidden = query.show_hidden.unwrap_or(false);
    let mut directories = fs::read_dir(&directory)?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !show_hidden && name.starts_with('.') {
                return None;
            }
            let file_type = entry.file_type().ok()?;
            (file_type.is_dir() || (file_type.is_symlink() && entry.path().is_dir())).then(|| {
                json!({
                    "name": name,
                    "path": entry.path(),
                })
            })
        })
        .collect::<Vec<_>>();
    directories.sort_by(|left, right| {
        left.get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_ascii_lowercase()
            .cmp(
                &right
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_ascii_lowercase(),
            )
    });
    directories.truncate(500);
    Ok(Json(json!({
        "ok": true,
        "path": directory,
        "parent": directory.parent().map(Path::to_path_buf),
        "directories": directories,
    }))
    .into_response())
}

async fn api_pending_approvals(State(state): State<AppState>) -> Json<Value> {
    Json(state.pending_approvals.public_view().await)
}

async fn api_answer_approval(
    State(state): State<AppState>,
    AxumPath(id): AxumPath<String>,
    Json(payload): Json<ApprovalAnswerPayload>,
) -> Response {
    match state.pending_approvals.answer(&id, payload).await {
        Ok(value) => Json(value).into_response(),
        Err(error) => (
            error.status_code(),
            Json(json!({"ok": false, "error": error.to_string()})),
        )
            .into_response(),
    }
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
                "has_api_key": config.provider_api_key(provider.id).is_some(),
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
            "sandbox_mode": config.web_sandbox_mode,
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
    let api_key = state
        .config
        .read()
        .await
        .resolve_provider_api_key(provider_id, supplied_key)
        .or_else(|| {
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
        }
        if let Some(mode) = payload.get("sandbox_mode").and_then(Value::as_str) {
            config.web_sandbox_mode = mode.to_string();
        }
        config.normalize();
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
    let models = state
        .llama
        .list_models(&cfg)
        .await
        .unwrap_or_else(|_| llama::known_models(&cfg.provider_id));
    Json(json!(llama::model_ids(models, &cfg.default_model)))
}

async fn api_preview_models(
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
                "error": "Account providers expose models through the terminal agent"
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
    let api_key = state
        .config
        .read()
        .await
        .resolve_provider_api_key(provider_id, supplied_key)
        .or_else(|| {
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

    let selection = match ProviderSelection::from_choice(provider_id.to_string(), api_key, base_url)
    {
        Ok(selection) => selection,
        Err(_) => {
            let models = llama::model_ids(llama::known_models(provider_id), provider.default_model);
            return Ok(Json(json!(models)).into_response());
        }
    };
    let mut cfg = state.config.read().await.clone();
    apply_provider_selection(&selection, &mut cfg);
    let models = state
        .llama
        .list_models(&cfg)
        .await
        .unwrap_or_else(|_| llama::known_models(provider_id));
    Ok(Json(json!(llama::model_ids(models, &selection.model))).into_response())
}

async fn api_get_memory(State(state): State<AppState>) -> Json<Value> {
    let cfg = state.config.read().await.clone();
    let facts = state.memory.list_facts(200, false).unwrap_or_default();
    let status = state
        .memory
        .status(&cfg)
        .ok()
        .and_then(|status| serde_json::to_value(status).ok())
        .unwrap_or(Value::Null);
    Json(json!({"facts": facts, "status": status}))
}

async fn api_memory_status(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let cfg = state.config.read().await.clone();
    let status = state.memory.status(&cfg)?;
    Ok(Json(json!({
        "report": status,
        "text": status.render_text(),
    })))
}

async fn api_memory_dream(
    State(state): State<AppState>,
    payload: Option<Json<Value>>,
) -> Result<Json<Value>, AppError> {
    let dry_run = payload
        .as_ref()
        .and_then(|Json(value)| value.get("dry_run"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    match state.dream.run(dry_run).await {
        Ok(report) => Ok(Json(json!({
            "ok": true,
            "text": report.render_text(),
            "report": report,
        }))),
        Err(err) => Ok(Json(json!({"ok": false, "error": err.to_string()}))),
    }
}

async fn api_memory_reindex(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let cfg = state.config.read().await.clone();
    let report = state.memory.reindex(&cfg).await?;
    Ok(Json(json!({
        "ok": true,
        "text": report.render_text(),
        "report": report,
    })))
}

async fn api_memory_forget(
    State(state): State<AppState>,
    Json(payload): Json<Value>,
) -> Result<Json<Value>, AppError> {
    let id = payload
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if id.is_empty() {
        return Ok(Json(json!({"ok": false, "error": "id required"})));
    }
    let forgotten = state.memory.forget(id)?;
    Ok(Json(json!({"ok": forgotten})))
}

async fn api_memory_clear(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    state.memory.clear()?;
    Ok(Json(json!({"ok": true})))
}

async fn api_skills(State(state): State<AppState>) -> Json<Value> {
    let workspace = current_workspace(&state).await;
    Json(json!({
        "skills": skills::discover(&workspace),
        "workspace": workspace,
    }))
}

async fn api_skill_inspect(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
) -> Json<Value> {
    let workspace = current_workspace(&state).await;
    match skills::load(&workspace, &name) {
        Ok(skill) => Json(json!({
            "ok": true,
            "skill": skill.summary,
            "instructions": skill.body,
        })),
        Err(error) => Json(json!({"ok": false, "error": error.to_string()})),
    }
}

async fn api_skill_install(
    State(state): State<AppState>,
    Json(payload): Json<Value>,
) -> Json<Value> {
    let source = payload
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    if source.is_empty() {
        return Json(json!({"ok": false, "error": "source required"}));
    }
    let workspace = current_workspace(&state).await;
    match tokio::task::spawn_blocking(move || skills::install(&source, &workspace)).await {
        Ok(Ok(skill)) => Json(json!({"ok": true, "skill": skill})),
        Ok(Err(error)) => Json(json!({"ok": false, "error": error.to_string()})),
        Err(error) => Json(json!({"ok": false, "error": error.to_string()})),
    }
}

async fn api_skill_update(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
) -> Json<Value> {
    let workspace = current_workspace(&state).await;
    match tokio::task::spawn_blocking(move || skills::update(&name, &workspace)).await {
        Ok(Ok(skill)) => Json(json!({"ok": true, "skill": skill})),
        Ok(Err(error)) => Json(json!({"ok": false, "error": error.to_string()})),
        Err(error) => Json(json!({"ok": false, "error": error.to_string()})),
    }
}

async fn api_skill_verify(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
) -> Json<Value> {
    let workspace = current_workspace(&state).await;
    match skills::verify(&workspace, &name) {
        Ok(report) => Json(json!({"ok": true, "report": report})),
        Err(error) => Json(json!({"ok": false, "error": error.to_string()})),
    }
}

async fn api_skill_remove(
    State(_state): State<AppState>,
    AxumPath(name): AxumPath<String>,
) -> Json<Value> {
    match skills::remove(&name) {
        Ok(()) => Json(json!({"ok": true})),
        Err(error) => Json(json!({"ok": false, "error": error.to_string()})),
    }
}

async fn api_skill_activate(
    State(state): State<AppState>,
    AxumPath((name, cid)): AxumPath<(String, String)>,
) -> Result<Json<Value>, AppError> {
    let workspace = current_workspace(&state).await;
    let skill = match skills::load(&workspace, &name) {
        Ok(skill) => skill,
        Err(error) => return Ok(Json(json!({"ok": false, "error": error.to_string()}))),
    };
    let chat = load_chat(&state.paths, &cid)?;
    let already_active = chat.messages.iter().any(|message| {
        message.role == "system"
            && message.extra.get("type").and_then(Value::as_str) == Some("skill_activation")
            && message.extra.get("skill").and_then(Value::as_str) == Some(name.as_str())
    });
    if !already_active {
        let cfg = state.config.read().await.clone();
        let mut extra = Map::new();
        extra.insert("type".into(), json!("skill_activation"));
        extra.insert("skill".into(), json!(name));
        append_msg(
            &state.paths,
            &cfg,
            &cid,
            "system",
            Value::String(skills::render_for_model(&skill)),
            extra,
        )?;
    }
    Ok(Json(json!({"ok": true, "active": skill.summary.name})))
}

async fn api_runtime_profile(State(state): State<AppState>) -> Json<Value> {
    let paths = runtime_paths(&state).await;
    Json(RuntimeProfile::detect(&paths).as_json())
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
    // The conversation is gone, so its memory summary must go too — chat ids
    // are reused, and a stale summary would resurface under a new chat.
    if let Err(err) = state.memory.forget_conversation(&cid) {
        warn!("could not drop memory summary for {cid}: {err}");
    }
    // Switch to an existing conversation; only an empty list justifies
    // creating one, otherwise every delete would spawn a new chat.
    let active = match list_chats(&state.paths)?.first() {
        Some(existing) => existing.id.clone(),
        None => new_chat(&state.paths)?.id,
    };
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
    if cfg.memory_enabled {
        state.memory.update_recent_conversation(&chat, &cfg)?;
    }
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

async fn api_agents(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    Ok(Json(workflow_tasks::agent_list(&state.paths, None)?))
}

async fn api_launch_agent(
    State(state): State<AppState>,
    Json(payload): Json<Value>,
) -> Result<Response, AppError> {
    let prompt = payload
        .get("prompt")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let Some(prompt) = prompt else {
        return Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "Subagent prompt is required"})),
        )
            .into_response());
    };
    let description = payload
        .get("description")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("Subagent manual");
    let subagent_type = payload
        .get("subagent_type")
        .and_then(Value::as_str)
        .unwrap_or("general-purpose");
    let provider_id = payload
        .get("provider_id")
        .and_then(Value::as_str)
        .unwrap_or("inherit");
    let model = payload
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let scope = payload
        .get("scope")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("manual");
    let cfg = state.config.read().await.clone();
    let execution_paths = runtime_paths(&state).await;
    let runtime_profile = RuntimeProfile::detect(&execution_paths);
    let memory_block = cfg
        .memory_enabled
        .then(|| state.memory.agent_memory_block(&cfg));
    match crate::tools::launch_background_subagent(
        &state.llama,
        &cfg,
        &execution_paths,
        &cfg.default_model,
        &runtime_profile,
        &state.pending_questions,
        &state.pending_approvals,
        &state.runtime,
        Some(state.config.clone()),
        state.local_web,
        scope,
        description,
        prompt,
        subagent_type,
        provider_id,
        model,
        memory_block,
    )
    .await
    {
        Ok(result) => Ok(Json(result).into_response()),
        Err(error) => Ok((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": error.to_string()})),
        )
            .into_response()),
    }
}

async fn api_agent(
    State(state): State<AppState>,
    AxumPath(agent_id): AxumPath<String>,
) -> Result<Response, AppError> {
    let value = workflow_tasks::agent_get(&state.paths, &agent_id)?;
    if value.get("agent").is_none_or(Value::is_null) {
        return Ok((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "Subagent not found"})),
        )
            .into_response());
    }
    Ok(Json(value).into_response())
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

    let _upload_guard = state.upload_lock.lock().await;
    ensure_upload_capacity(&state.paths.uploads_dir, data.len())?;
    let dest = write_unique_private(&state.paths.uploads_dir, &filename, &data)?;
    drop(_upload_guard);
    let read = read_file(&dest).await?;

    let cfg = state.config.read().await.clone();
    let extracted = transcribe_media(&read, &cfg, state.llama.http()).await;
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
    if cfg.memory_enabled {
        state.memory.update_recent_conversation(&chat, &cfg)?;
    }

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
    let read_path = path.clone();
    let bytes = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<u8>> {
        let metadata = fs::symlink_metadata(&read_path)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            anyhow::bail!("download target is not a regular file")
        }
        let mut file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&read_path)?;
        let mut bytes = Vec::with_capacity(metadata.len().min(50 * 1024 * 1024) as usize);
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    })
    .await
    .map_err(anyhow::Error::from)??;
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
    let execution_paths = runtime_paths(&state).await;
    let (model, _) = resolve_model(&state.llama, &cfg, payload.model.as_deref()).await;
    match generate_document(
        &state.llama,
        &cfg,
        &execution_paths,
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

/// The live turn, as the browser sees it.
///
/// The work runs in a spawned task rather than inline in the stream body so
/// that events reach the browser while tools are still executing. Inline, the
/// tool loop would have to finish before the first `yield`, which is exactly
/// the behaviour this replaces.
async fn api_stream(
    State(state): State<AppState>,
    AxumPath((cid, _mid)): AxumPath<(String, String)>,
    Query(params): Query<StreamQuery>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let (turn_id, cancel) = state.turns.begin(&cid).await;
    let (turn, mut events) = TurnStream::new(cancel);

    let worker_state = state.clone();
    let worker_turn = turn.clone();
    let worker_cid = cid.clone();
    let worker = tokio::spawn(async move {
        let cfg = worker_state.config.read().await.clone();
        let chat = load_chat(&worker_state.paths, &worker_cid)?;
        let execution_paths = runtime_paths(&worker_state).await;
        let runtime_profile = RuntimeProfile::detect(&execution_paths);
        let (model, known_models) =
            resolve_model(&worker_state.llama, &cfg, params.model.as_deref()).await;
        let plan = prepare_streaming_response_with_uploads(
            &worker_state.llama,
            &cfg,
            &execution_paths,
            Some(worker_state.memory.clone()),
            &runtime_profile,
            &model,
            &known_models,
            &params.query,
            &chat.messages,
            Some(&worker_cid),
            &worker_state.pending_questions,
            &worker_state.pending_approvals,
            &worker_state.runtime,
            Some(worker_state.config.clone()),
            worker_state.local_web,
            &worker_turn,
        )
        .await;

        let answer = match plan {
            // Vision and uploaded-file turns have no tool loop, so their text
            // is streamed straight from the provider here.
            StreamResponsePlan::Direct {
                messages,
                temperature,
            } => {
                let mut tokens = worker_state
                    .llama
                    .chat_stream(&cfg, &model, messages, temperature)
                    .await?;
                let mut answer = String::new();
                loop {
                    let next = tokio::select! {
                        biased;
                        _ = worker_turn.cancel_token().cancelled() => None,
                        next = tokens.next() => next,
                    };
                    let Some(token) = next else { break };
                    let token = token?;
                    if token.is_empty() {
                        continue;
                    }
                    answer.push_str(&token);
                    worker_turn.emit(TurnEvent::Token { text: token }).await;
                }
                answer
            }
            // The tool loop already streamed its tokens through `worker_turn`.
            StreamResponsePlan::Fallback(reply) => reply,
        };

        worker_turn
            .emit_web(WebEvent::Answer {
                text: answer.clone(),
            })
            .await;
        Ok::<String, anyhow::Error>(answer)
    });

    Sse::new(stream! {
        while let Some(event) = events.recv().await {
            yield Ok(Event::default().data(event.to_string()));
        }
        // The sender is dropped when the worker ends, so this never blocks on
        // a turn that is still running.
        match worker.await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                yield Ok(Event::default().data(
                    json!({"event": "error", "message": error.to_string(), "fatal": true})
                        .to_string(),
                ));
            }
            Err(error) => {
                yield Ok(Event::default().data(
                    json!({"event": "error", "message": format!("turn failed: {error}"), "fatal": true})
                        .to_string(),
                ));
            }
        }
        state.turns.finish(&cid, turn_id).await;
        yield Ok(Event::default().data("[DONE]"));
    })
}

async fn api_interrupt(
    State(state): State<AppState>,
    AxumPath(cid): AxumPath<String>,
) -> Json<Value> {
    Json(json!({"ok": true, "interrupted": state.turns.cancel(&cid).await}))
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

async fn api_whatsapp_events(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut receiver = state.whatsapp_events.subscribe();
    let cfg = state.config.read().await.clone();
    let initial = state.whatsapp.status(&cfg, &state.paths).await;
    Sse::new(stream! {
        yield Ok(Event::default().data(initial.to_string()));
        loop {
            match receiver.recv().await {
                Ok(status) => yield Ok(Event::default().data(status.to_string())),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    })
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

async fn api_whatsapp_qr_refresh(State(state): State<AppState>) -> Result<Response, AppError> {
    let cfg = state.config.read().await.clone();
    match state.whatsapp.restart_for_pairing(&cfg, &state.paths).await {
        Ok(message) => Ok(Json(json!({
            "ok": true,
            "message": message,
            "qr_regenerating": true
        }))
        .into_response()),
        Err(error) => Ok((
            StatusCode::CONFLICT,
            Json(json!({
                "ok": false,
                "error": error.to_string()
            })),
        )
            .into_response()),
    }
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

    // Handle quick commands from WhatsApp
    if content.starts_with('/') {
        let reply = handle_whatsapp_command(&state, &cid, &content).await;
        if let Some(reply_text) = reply {
            send_whatsapp_message(&cfg, chat_jid, &reply_text).await;
            return Ok(Json(json!({"ok": true, "command_handled": true})));
        }
    }

    // Any attachment kind the bridge forwards — images go through OCR, every
    // other type through the document text extractors.
    if let Some(media) = payload.get("media").and_then(Value::as_object) {
        let _upload_guard = state.upload_lock.lock().await;
        let media_type = media
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("document");
        let b64 = media
            .get("data_base64")
            .or_else(|| media.get("data"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if !b64.is_empty() {
            let filename = media.get("filename").and_then(Value::as_str);
            let mimetype = media.get("mimetype").and_then(Value::as_str);
            let is_image = media_type == "image";
            let (final_name, path, read) = save_base64_media(
                &state.paths,
                &state.paths.whatsapp_media_dir,
                filename,
                mimetype,
                b64,
                is_image,
            )
            .await?;
            drop(_upload_guard);
            let extracted = transcribe_media(&read, &cfg, state.llama.http()).await;
            let meta = if read.file_type == "image" {
                image_meta(&final_name, &path, &extracted)
            } else {
                json!({
                    "type": read.file_type,
                    "filename": final_name,
                    "path": path.to_string_lossy(),
                })
            };
            append_msg(&state.paths, &cfg, &cid, "user", meta, extra.clone())?;
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
            // A video's still frame is stored as its own image message, so a
            // vision-capable model sees the scene while the transcript
            // carries the speech.
            if read.file_type == "video" {
                if let Some(thumb_b64) = media
                    .get("thumbnail_base64")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                {
                    let thumbnail = {
                        let _thumbnail_guard = state.upload_lock.lock().await;
                        save_base64_media(
                            &state.paths,
                            &state.paths.whatsapp_media_dir,
                            Some(&format!("{final_name}-thumbnail.jpg")),
                            Some("image/jpeg"),
                            thumb_b64,
                            true,
                        )
                        .await
                    };
                    match thumbnail {
                        Ok((thumb_name, thumb_path, thumb_read)) => {
                            append_msg(
                                &state.paths,
                                &cfg,
                                &cid,
                                "user",
                                image_meta(&thumb_name, &thumb_path, &thumb_read.ocr),
                                extra.clone(),
                            )?;
                        }
                        Err(err) => warn!("could not store video thumbnail: {err}"),
                    }
                }
            }

            if content.is_empty() {
                content = match read.file_type.as_str() {
                    "image" => format!("Analizeaza imaginea atasata: {final_name}"),
                    "audio" => format!("Asculta mesajul vocal atasat: {final_name}"),
                    "video" => format!("Analizeaza videoclipul atasat: {final_name}"),
                    _ => format!("Analizeaza fisierul atasat: {final_name}"),
                };
            }
        }
    }
    if content.is_empty() {
        content = "Am primit un atasament WhatsApp pe care nu il pot procesa inca.".into();
    }

    if content.eq_ignore_ascii_case("/skills") {
        let workspace = current_workspace(&state).await;
        let reply = skills::render_catalog(&workspace);
        send_whatsapp_message(&cfg, chat_jid, &reply).await;
        return Ok(Json(
            json!({"ok": true, "reply": reply, "command": "skills"}),
        ));
    }
    if let Some(arguments) = content.strip_prefix("/skill ") {
        let workspace = current_workspace(&state).await;
        let (action, value) = arguments
            .trim()
            .split_once(char::is_whitespace)
            .map(|(action, value)| (action.to_ascii_lowercase(), value.trim()))
            .unwrap_or((arguments.trim().to_ascii_lowercase(), ""));
        let reply = match action.as_str() {
            "use" | "activate" if !value.is_empty() => match skills::load(&workspace, value) {
                Ok(skill) => {
                    let mut skill_extra = Map::new();
                    skill_extra.insert("channel".into(), json!("whatsapp"));
                    skill_extra.insert("type".into(), json!("skill_activation"));
                    skill_extra.insert("skill".into(), json!(skill.summary.name));
                    append_msg(
                        &state.paths,
                        &cfg,
                        &cid,
                        "system",
                        Value::String(skills::render_for_model(&skill)),
                        skill_extra,
                    )?;
                    format!(
                        "Skillul `{}` este activ în acest chat. Permisiunile nu au fost \
                             modificate.",
                        skill.summary.name
                    )
                }
                Err(error) => format!("Nu pot activa skillul: {error}"),
            },
            "inspect" | "show" if !value.is_empty() => skills::inspect(&workspace, value)
                .unwrap_or_else(|error| format!("Nu pot inspecta skillul: {error}")),
            "install" | "update" | "remove" | "uninstall" => {
                "Din motive de securitate, instalarea și ștergerea skillurilor se confirmă \
                 local în Agent sau WebTool."
                    .into()
            }
            _ => "Folosește `/skills`, `/skill use NUME` sau `/skill inspect NUME`.".into(),
        };
        send_whatsapp_message(&cfg, chat_jid, &reply).await;
        return Ok(Json(
            json!({"ok": true, "reply": reply, "command": "skill"}),
        ));
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
    let execution_paths = runtime_paths(&state).await;
    let runtime_profile = RuntimeProfile::detect(&execution_paths);
    let (model, known_models) = resolve_model(&state.llama, &cfg, None).await;
    let reply = generate_chat_response_with_uploads(
        &state.llama,
        &cfg,
        &execution_paths,
        Some(state.memory.clone()),
        &runtime_profile,
        &model,
        &known_models,
        &content,
        &chat.messages,
        Some(&cid),
        &state.pending_questions,
        &state.pending_approvals,
        &state.runtime,
        Some(state.config.clone()),
        state.local_web,
        // WhatsApp delivers one finished message; there is nothing to stream
        // progress into and no browser connection to interrupt.
        &TurnStream::detached(),
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
    if cfg.memory_enabled {
        state
            .memory
            .update_recent_conversation(&updated_chat, &cfg)?;
    }
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
    config.remember_provider_api_key(&selection.provider_id, selection.api_key());
    config.llama_api_key = selection.api_key().unwrap_or_default().to_string();
    if let Some(base_url) = selection.resolved_base_url() {
        config.llama_base_url = base_url.to_string();
    }
}

/// Fire-and-forget fact extraction after a finished answer. The channel is
/// recorded per fact so provenance survives across interfaces.
fn spawn_memory_refresh(state: AppState, chat: crate::storage::Chat, model: String) {
    tokio::spawn(async move {
        let cfg = state.config.read().await.clone();
        let channel = if chat.id.starts_with("wa_") {
            "whatsapp"
        } else {
            "webtool"
        };
        if let Err(err) = state
            .memory
            .extract_from_chat(&state.llama, &cfg, &model, &chat, channel)
            .await
        {
            warn!("Memory refresh failed for {}: {err}", chat.id);
        }
    });
}

/// Handle quick commands from WhatsApp
async fn handle_whatsapp_command(state: &AppState, chat_id: &str, command: &str) -> Option<String> {
    let parts: Vec<&str> = command.split_whitespace().collect();
    let cmd = parts.first()?;

    match *cmd {
        "/remember" => {
            let fact = parts.get(1..)?.join(" ");
            if fact.is_empty() {
                return Some("Usage: /remember <fact to remember>".to_string());
            }

            let new_fact = crate::memory_engine::NewFact {
                text: fact.clone(),
                category: crate::memory::MemoryCategory::UserPreference,
                confidence: 0.9,
                importance: 0.8,
                source_chat_id: chat_id.to_string(),
                source_channel: "whatsapp".to_string(),
                sources: vec!["whatsapp_command".to_string()],
            };

            match state
                .memory
                .apply_ops(&[crate::memory_engine::MemOp::Add(new_fact)])
            {
                Ok((_, changed)) => {
                    if let Some(id) = changed.first() {
                        Some(format!("✓ Remembered: {} (ID: {})", fact, id))
                    } else {
                        Some(format!("✓ Remembered: {}", fact))
                    }
                }
                Err(e) => Some(format!("✗ Failed to remember: {}", e)),
            }
        }

        "/forget" => {
            let Some(id) = parts.get(1) else {
                return Some("Usage: /forget <memory id>".to_string());
            };
            match state.memory.forget(id) {
                Ok(true) => Some(format!("✓ Forgot: {}", id)),
                Ok(false) => Some(format!("✗ Memory not found: {}", id)),
                Err(e) => Some(format!("✗ Failed to forget: {}", e)),
            }
        }

        "/memories" => match state.memory.list_facts(50, false) {
            Ok(facts) => {
                if facts.is_empty() {
                    return Some("No memories stored.".to_string());
                }
                let mut reply = String::from("📝 Stored memories:\n\n");
                for (i, fact) in facts.iter().take(10).enumerate() {
                    reply.push_str(&format!("{}. {} ({})\n", i + 1, fact.text, fact.id));
                }
                if facts.len() > 10 {
                    reply.push_str(&format!("\n... and {} more", facts.len() - 10));
                }
                Some(reply)
            }
            Err(e) => Some(format!("✗ Failed to list memories: {}", e)),
        },

        "/status" => {
            let cfg = state.config.read().await;
            let bridge_running = state.whatsapp.is_running().await;
            let workspace = current_workspace(state).await;
            let status_text = if bridge_running {
                "🟢 Bridge running"
            } else {
                "🔴 Bridge stopped"
            };
            Some(format!(
                "WhatsApp Status: {}\nAssistant: {}\nProvider: {}\nModel: {}\nFolder: {}\nSandbox: {}",
                status_text,
                cfg.whatsapp_assistant_name,
                cfg.provider_id,
                cfg.default_model,
                workspace.display(),
                cfg.web_sandbox_mode,
            ))
        }

        "/workspace" => {
            let workspace = current_workspace(state).await;
            Some(format!(
                "📁 Folderul comun WebTool + WhatsApp este:\n{}\n\nSchimbă-l din selectorul local WebTool.",
                workspace.display()
            ))
        }

        "/sandbox" => {
            let cfg = state.config.read().await;
            Some(format!(
                "🛡️ Mod comun: {}\nAprobările pentru scrieri/comenzi și autentificarea sudo se fac local în WebTool.",
                cfg.web_sandbox_mode
            ))
        }

        "/agents" => match workflow_tasks::agent_list(&state.paths, None) {
            Ok(value) => {
                let agents = value
                    .get("agents")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                if agents.is_empty() {
                    return Some("Nu există încă subagenți.".into());
                }
                let mut reply = format!(
                    "🤖 Subagenți comuni — {} activi / {} total\n\n",
                    value.get("running").and_then(Value::as_u64).unwrap_or(0),
                    agents.len()
                );
                for agent in agents.iter().take(12) {
                    let status = agent.get("status").and_then(Value::as_str).unwrap_or("?");
                    let icon = match status {
                        "running" | "in_progress" | "pending" => "🟡",
                        "completed" => "🟢",
                        "failed" => "🔴",
                        "killed" | "cancelled" => "⚫",
                        _ => "⚪",
                    };
                    let metadata = agent.get("metadata").and_then(Value::as_object);
                    let profile = metadata
                        .and_then(|meta| meta.get("agent_type"))
                        .and_then(Value::as_str)
                        .unwrap_or("general-purpose");
                    let provider = metadata
                        .and_then(|meta| meta.get("provider_id"))
                        .and_then(Value::as_str)
                        .unwrap_or("inherit");
                    reply.push_str(&format!(
                        "{} {} · {} · {}/{} · {}\n   {}\n",
                        icon,
                        agent.get("id").and_then(Value::as_str).unwrap_or("?"),
                        profile,
                        provider,
                        agent
                            .get("model")
                            .and_then(Value::as_str)
                            .unwrap_or("inherit"),
                        status,
                        agent
                            .get("subject")
                            .and_then(Value::as_str)
                            .unwrap_or("Subagent")
                    ));
                }
                reply.push_str("\nDetalii: /agent ID · Oprire: /stopagent ID");
                Some(reply)
            }
            Err(error) => Some(format!("Nu pot citi subagenții: {error}")),
        },

        "/agent" => {
            let Some(agent_id) = parts.get(1) else {
                return Some("Folosește /agent ID".into());
            };
            match workflow_tasks::agent_get(&state.paths, agent_id) {
                Ok(value) => {
                    let Some(agent) = value.get("agent").filter(|value| !value.is_null()) else {
                        return Some(format!("Subagentul {agent_id} nu există."));
                    };
                    let output = agent.get("output").and_then(Value::as_str).unwrap_or("");
                    let mut preview = output.chars().rev().take(3_500).collect::<String>();
                    preview = preview.chars().rev().collect();
                    Some(format!(
                        "🤖 {}\nStare: {}\nTip: {}\nProvider/model: {}/{}\nTask: {}\n\n{}",
                        agent.get("id").and_then(Value::as_str).unwrap_or(agent_id),
                        agent.get("status").and_then(Value::as_str).unwrap_or("?"),
                        agent
                            .get("agentType")
                            .and_then(Value::as_str)
                            .unwrap_or("general-purpose"),
                        agent
                            .get("metadata")
                            .and_then(|meta| meta.get("provider_id"))
                            .and_then(Value::as_str)
                            .unwrap_or("inherit"),
                        agent
                            .get("model")
                            .and_then(Value::as_str)
                            .unwrap_or("inherit"),
                        agent
                            .get("subject")
                            .and_then(Value::as_str)
                            .unwrap_or("Subagent"),
                        if preview.trim().is_empty() {
                            "Fără output încă."
                        } else {
                            preview.trim()
                        }
                    ))
                }
                Err(error) => Some(format!("Nu pot citi subagentul: {error}")),
            }
        }

        "/stopagent" => {
            let Some(agent_id) = parts.get(1) else {
                return Some("Folosește /stopagent ID".into());
            };
            let status = workflow_tasks::agent_get(&state.paths, agent_id)
                .ok()
                .and_then(|value| value.get("agent").cloned())
                .and_then(|agent| {
                    agent
                        .get("status")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                });
            if status.is_none() {
                return Some(format!("Subagentul {agent_id} nu există."));
            }
            if status.as_deref().is_some_and(|status| {
                matches!(status, "completed" | "failed" | "killed" | "cancelled")
            }) {
                return Some(format!(
                    "Subagentul {agent_id} este deja {}.",
                    status.unwrap()
                ));
            }
            match workflow_tasks::task_stop(&state.paths, chat_id, agent_id) {
                Ok(_) => {
                    let stopped = state.runtime.stop(agent_id).await;
                    Some(format!(
                        "{} Subagentul {agent_id} a fost oprit.",
                        if stopped { "✓" } else { "⚠" }
                    ))
                }
                Err(error) => Some(format!("Nu pot opri subagentul: {error}")),
            }
        }

        "/restart" => {
            let cfg = state.config.read().await.clone();
            state.whatsapp.stop(&cfg).await;
            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
            match state.whatsapp.start(&cfg, &state.paths, true).await {
                Ok(msg) => Some(format!("🔄 Bridge restarted: {}", msg)),
                Err(e) => Some(format!("✗ Restart failed: {}", e)),
            }
        }

        "/help" => Some(
            "🤖 Available commands:\n\n\
                /remember <fact> - Store a fact in memory\n\
                /forget <id> - Remove a memory\n\
                /memories - List all memories\n\
                /status - Show WhatsApp and AI status\n\
                /workspace - Show the shared working folder\n\
                /sandbox - Show the shared execution policy\n\
                /agents - List shared subagents\n\
                /agent <id> - Show a subagent and its output\n\
                /stopagent <id> - Stop a running subagent\n\
                /restart - Restart the WhatsApp bridge\n\
                /help - Show this help message"
                .to_string(),
        ),

        _ => None, // Not a recognized command, let normal processing continue
    }
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

    #[test]
    fn provider_key_map_is_never_exposed_to_the_browser() {
        let mut config = AppConfig::default();
        config.remember_provider_api_key("openai", Some("sk-browser-must-not-see"));
        config.llama_api_key = "sk-browser-must-not-see".into();

        let value = public_config_value(&config);
        let serialized = serde_json::to_string(&value).unwrap();
        assert!(value.get("provider_api_keys").is_none());
        assert_eq!(value["llama_api_key"], json!(""));
        assert!(!serialized.contains("sk-browser-must-not-see"));
    }
}
