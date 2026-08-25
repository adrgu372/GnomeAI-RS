//! Native graphical interface for the coding agent.
//!
//! The GUI deliberately speaks the same `Op`/`Event` protocol as the former
//! terminal interface. Agent execution, approvals, sessions, memory, skills,
//! sandboxing and provider selection remain core concerns; this module only
//! owns presentation and desktop input.

use anyhow::{Result, anyhow};
use base64::Engine as _;
use eframe::egui::{
    self, Align, Color32, FontFamily, FontId, Key, KeyboardShortcut, Layout, Margin, Modifiers,
    RichText, ScrollArea, Sense, Stroke, TextEdit, Vec2,
};
use qrcode::{QrCode, types::Color as QrColor};
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};
use tokio::runtime::Handle;
use tokio::sync::mpsc;

use crate::config::{AppConfig, McpServerConfig, McpTransport};
use crate::protocol::{Decision, Event, Op, SecretString, SessionSummary};
use crate::provider_catalog::{AuthKind, PROVIDERS};
use crate::uploads::{
    IMAGE_EXTS, OOXML_EXTS, TEXT_EXTS, extract_text_attachment, file_type_from_name,
};

const COMMANDS: &[(&str, &str)] = &[
    ("/help", "Show commands and keyboard shortcuts"),
    ("/new", "Start a fresh session"),
    ("/sessions", "Open saved sessions"),
    ("/resume", "Resume a session by ID"),
    ("/fork", "Branch the current session"),
    ("/compact", "Compact context now"),
    ("/rollback", "Undo patches in this session"),
    ("/workspace", "Choose a project folder"),
    ("/cd", "Alias for /workspace"),
    ("/provider", "Choose provider or account login"),
    ("/model", "Choose the active model"),
    ("/websearch", "Toggle web search"),
    ("/whatsapp", "Open separate WhatsApp conversations"),
    ("/nodes", "Manage paired lightweight devices"),
    ("/sandbox", "Set read-only, normal or full-access"),
    ("/skills", "List installed skills"),
    ("/skill", "Use, inspect, install, update or remove a skill"),
    ("/memory", "Show shared memory"),
    ("/copy", "Copy the last assistant reply"),
    ("/contrast", "Toggle high-contrast colors"),
    ("/notify", "Toggle desktop notifications"),
    ("/mouse", "Mouse input is always enabled in the GUI"),
    ("/tokens", "Show session token use"),
    ("/doctor", "Run diagnostics"),
    ("/diff", "Show the accumulated diff"),
    ("/export", "Export the transcript to Markdown"),
    ("/clear", "Clear only the visible transcript"),
    ("/quit", "Close GnomeAI-RS"),
];

const MAX_ATTACHMENT_BYTES: usize = 20 * 1024 * 1024;

#[derive(Debug)]
enum Block {
    User(String),
    Assistant(String),
    Reasoning(String),
    Tool {
        call_id: String,
        name: String,
        summary: String,
        output: String,
        done: bool,
        ok: bool,
        ms: u64,
    },
    Diff(String),
    Verify {
        stage: String,
        passed: bool,
        summary: String,
    },
    Error(String),
    Note(String),
}

impl Block {
    fn searchable_text(&self) -> String {
        match self {
            Self::User(text)
            | Self::Assistant(text)
            | Self::Reasoning(text)
            | Self::Diff(text)
            | Self::Error(text)
            | Self::Note(text) => text.clone(),
            Self::Tool {
                name,
                summary,
                output,
                ..
            } => format!("{name} {summary} {output}"),
            Self::Verify { stage, summary, .. } => format!("{stage} {summary}"),
        }
    }
}

#[derive(Debug)]
struct QueuedMessage {
    text: String,
    attachment: Option<PathBuf>,
}

#[derive(Debug)]
struct ApprovalDialog {
    call_id: String,
    command: String,
    reason: String,
    allow_always: bool,
}

#[derive(Debug)]
struct PrivilegeDialog {
    request_id: String,
    command: String,
    credential: String,
    remember: bool,
    keyring_available: bool,
    attempt: u8,
    prompt: Option<String>,
    dynamic: bool,
    message: Option<String>,
}

enum LoginUpdate {
    DeviceCode {
        verification_url: String,
        user_code: String,
    },
    Finished {
        provider_id: String,
        result: std::result::Result<(), String>,
    },
}

struct DeviceLoginDialog {
    verification_url: String,
    user_code: String,
    browser_error: Option<String>,
}

pub struct WhatsAppLaunchConfig {
    api_base: String,
    bridge_base: String,
    api_port: u16,
    bridge_port: u16,
    token: String,
    enabled: bool,
    assistant_name: String,
    has_own_number: bool,
    allowed_jids: Vec<String>,
    log_file: PathBuf,
    node_api_base: String,
    node_admin_token: String,
    node_enrollment_token: String,
    node_enabled: bool,
    node_bind: String,
    node_port: u16,
}

impl WhatsAppLaunchConfig {
    pub fn from_config(config: &AppConfig, app_home: &Path) -> Self {
        let token = format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let (api_port, bridge_port) = native_service_ports(config);
        Self {
            api_base: format!("http://127.0.0.1:{api_port}"),
            bridge_base: format!("http://127.0.0.1:{bridge_port}"),
            api_port,
            bridge_port,
            token,
            enabled: config.whatsapp_enabled,
            assistant_name: config.whatsapp_assistant_name.clone(),
            has_own_number: config.whatsapp_has_own_number,
            allowed_jids: config.whatsapp_allowed_jids.clone(),
            log_file: app_home.join("whatsapp_bridge.log"),
            node_api_base: format!("http://127.0.0.1:{}", config.node_hub_port),
            node_admin_token: config.node_hub_admin_token.clone(),
            node_enrollment_token: config.node_hub_token.clone(),
            node_enabled: config.node_hub_enabled,
            node_bind: config.node_hub_bind.clone(),
            node_port: config.node_hub_port,
        }
    }
}

struct WhatsAppService {
    child: Option<Child>,
    api_base: String,
    bridge_base: String,
    token: String,
    launch_error: Option<String>,
}

impl WhatsAppService {
    fn launch(config: &WhatsAppLaunchConfig) -> Self {
        let mut service = Self {
            child: None,
            api_base: config.api_base.clone(),
            bridge_base: config.bridge_base.clone(),
            token: config.token.clone(),
            launch_error: None,
        };
        match companion_executable("gnomef-whatsapp") {
            Ok(executable) => match Command::new(&executable)
                .env("GNOMEF_WEB_TOKEN", &config.token)
                .env("GNOMEF_NATIVE_HELPER", "1")
                .env("GNOMEF_NATIVE_API_PORT", config.api_port.to_string())
                .env("GNOMEF_NATIVE_BRIDGE_PORT", config.bridge_port.to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(child) => service.child = Some(child),
                Err(error) => {
                    service.launch_error = Some(format!(
                        "Cannot start WhatsApp service {}: {error}",
                        executable.display()
                    ));
                }
            },
            Err(error) => service.launch_error = Some(error),
        }
        service
    }
}

fn native_service_ports(config: &AppConfig) -> (u16, u16) {
    let api_listener = TcpListener::bind(("127.0.0.1", config.port))
        .or_else(|_| TcpListener::bind(("127.0.0.1", 0)));
    let api_port = api_listener
        .as_ref()
        .ok()
        .and_then(|listener| listener.local_addr().ok())
        .map(|address| address.port())
        .unwrap_or(config.port);

    // Keep the API listener alive while selecting the bridge port so the OS
    // cannot hand the same ephemeral port to both services.
    let bridge_listener = TcpListener::bind(("127.0.0.1", config.whatsapp_bridge_port))
        .or_else(|_| TcpListener::bind(("127.0.0.1", 0)));
    let bridge_port = bridge_listener
        .as_ref()
        .ok()
        .and_then(|listener| listener.local_addr().ok())
        .map(|address| address.port())
        .unwrap_or(config.whatsapp_bridge_port);

    (api_port, bridge_port)
}

impl Drop for WhatsAppService {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

enum WhatsAppReply {
    Status(std::result::Result<Value, String>),
    Action(std::result::Result<Value, String>),
    Sent(std::result::Result<Value, String>),
    Conversations(std::result::Result<WhatsAppConversationSnapshot, String>),
}

#[derive(Debug)]
struct WhatsAppConversationSnapshot {
    chats: Vec<(String, String)>,
    active_id: Option<String>,
    active_chat: Option<Value>,
}

enum NodeReply {
    List(std::result::Result<Value, String>),
    Policy(std::result::Result<Value, String>),
}

pub fn run(
    ops: mpsc::Sender<Op>,
    events: mpsc::Receiver<Event>,
    whatsapp: WhatsAppLaunchConfig,
) -> Result<()> {
    let runtime = Handle::current();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("GnomeAI-RS")
            .with_inner_size([1220.0, 780.0])
            .with_min_inner_size([880.0, 540.0]),
        centered: true,
        ..Default::default()
    };

    eframe::run_native(
        "GnomeAI-RS",
        options,
        Box::new(move |creation| {
            Ok(Box::new(GuiApp::new(
                creation, ops, events, runtime, whatsapp,
            )))
        }),
    )
    .map_err(|error| anyhow!(error.to_string()))
}

struct GuiApp {
    ops: mpsc::Sender<Op>,
    events: mpsc::Receiver<Event>,
    runtime: Handle,
    login_tx: std_mpsc::Sender<LoginUpdate>,
    login_rx: std_mpsc::Receiver<LoginUpdate>,
    whatsapp_tx: std_mpsc::Sender<WhatsAppReply>,
    whatsapp_rx: std_mpsc::Receiver<WhatsAppReply>,
    node_tx: std_mpsc::Sender<NodeReply>,
    node_rx: std_mpsc::Receiver<NodeReply>,
    whatsapp_service: WhatsAppService,

    blocks: Vec<Block>,
    composer: String,
    queue: VecDeque<QueuedMessage>,
    history: Vec<String>,
    history_pos: Option<usize>,
    pending_attachment: Option<PathBuf>,
    busy: bool,
    started: Option<Instant>,
    fatal: bool,

    session_id: String,
    provider: String,
    model: String,
    models: Vec<String>,
    workspace: PathBuf,
    branch: Option<String>,
    sandbox: String,
    web_search_enabled: bool,
    recent_workspaces: Vec<String>,
    tokens_in: i64,
    tokens_out: i64,
    token_history: Vec<(i64, i64, u64)>,

    approval: Option<ApprovalDialog>,
    privilege: Option<PrivilegeDialog>,
    sessions: Vec<SessionSummary>,
    show_sessions: bool,
    confirm_delete_session: Option<String>,
    rename_session: Option<(String, String)>,
    show_provider: bool,
    provider_index: usize,
    provider_api_key: String,
    provider_base_url: String,
    login_status: Option<String>,
    device_login: Option<DeviceLoginDialog>,
    show_models: bool,
    model_filter: String,
    show_help: bool,
    show_settings: bool,
    show_activity: bool,
    show_whatsapp: bool,
    show_whatsapp_conversations: bool,
    show_nodes: bool,
    mcp_servers: Vec<McpServerConfig>,
    whatsapp_enabled: bool,
    whatsapp_assistant_name: String,
    whatsapp_has_own_number: bool,
    whatsapp_allowed_jids: String,
    whatsapp_status: Value,
    whatsapp_feedback: Option<String>,
    whatsapp_request_pending: bool,
    whatsapp_last_poll: Instant,
    whatsapp_conversations: Vec<(String, String)>,
    whatsapp_selected_chat: Option<String>,
    whatsapp_active_chat: Option<Value>,
    whatsapp_conversations_pending: bool,
    whatsapp_conversations_last_poll: Instant,
    whatsapp_test_jid: String,
    whatsapp_test_message: String,
    whatsapp_log_file: PathBuf,
    node_status: Value,
    node_feedback: Option<String>,
    node_request_pending: bool,
    node_last_poll: Instant,
    node_api_base: String,
    node_admin_token: String,
    node_enrollment_token: String,
    node_enabled: bool,
    node_bind: String,
    node_port: u16,
    search: String,
    notifications: bool,
    high_contrast: bool,
    copy_request: Option<String>,
    quit_requested: bool,
    request_focus: bool,
}

impl GuiApp {
    fn new(
        creation: &eframe::CreationContext<'_>,
        ops: mpsc::Sender<Op>,
        events: mpsc::Receiver<Event>,
        runtime: Handle,
        whatsapp: WhatsAppLaunchConfig,
    ) -> Self {
        configure_style(&creation.egui_ctx);
        let (login_tx, login_rx) = std_mpsc::channel();
        let (whatsapp_tx, whatsapp_rx) = std_mpsc::channel();
        let (node_tx, node_rx) = std_mpsc::channel();
        let whatsapp_service = WhatsAppService::launch(&whatsapp);
        let whatsapp_allowed_jids = whatsapp.allowed_jids.join("\n");
        Self {
            ops,
            events,
            runtime,
            login_tx,
            login_rx,
            whatsapp_tx,
            whatsapp_rx,
            node_tx,
            node_rx,
            whatsapp_service,
            blocks: Vec::new(),
            composer: String::new(),
            queue: VecDeque::new(),
            history: Vec::new(),
            history_pos: None,
            pending_attachment: None,
            busy: false,
            started: None,
            fatal: false,
            session_id: String::new(),
            provider: "—".into(),
            model: "—".into(),
            models: Vec::new(),
            workspace: PathBuf::new(),
            branch: None,
            sandbox: "normal".into(),
            web_search_enabled: false,
            recent_workspaces: Vec::new(),
            tokens_in: 0,
            tokens_out: 0,
            token_history: Vec::new(),
            approval: None,
            privilege: None,
            sessions: Vec::new(),
            show_sessions: false,
            confirm_delete_session: None,
            rename_session: None,
            show_provider: false,
            provider_index: 0,
            provider_api_key: String::new(),
            provider_base_url: PROVIDERS
                .iter()
                .find(|provider| provider.id == "custom")
                .map(|provider| provider.base_url.to_string())
                .unwrap_or_default(),
            login_status: None,
            device_login: None,
            show_models: false,
            model_filter: String::new(),
            show_help: false,
            show_settings: false,
            show_activity: false,
            show_whatsapp: false,
            show_whatsapp_conversations: false,
            show_nodes: false,
            mcp_servers: Vec::new(),
            whatsapp_enabled: whatsapp.enabled,
            whatsapp_assistant_name: whatsapp.assistant_name,
            whatsapp_has_own_number: whatsapp.has_own_number,
            whatsapp_allowed_jids,
            whatsapp_status: serde_json::json!({}),
            whatsapp_feedback: None,
            whatsapp_request_pending: false,
            whatsapp_last_poll: Instant::now() - Duration::from_secs(10),
            whatsapp_conversations: Vec::new(),
            whatsapp_selected_chat: None,
            whatsapp_active_chat: None,
            whatsapp_conversations_pending: false,
            whatsapp_conversations_last_poll: Instant::now() - Duration::from_secs(10),
            whatsapp_test_jid: String::new(),
            whatsapp_test_message: String::new(),
            whatsapp_log_file: whatsapp.log_file,
            node_status: serde_json::json!({"nodes": []}),
            node_feedback: None,
            node_request_pending: false,
            node_last_poll: Instant::now() - Duration::from_secs(10),
            node_api_base: whatsapp.node_api_base,
            node_admin_token: whatsapp.node_admin_token,
            node_enrollment_token: whatsapp.node_enrollment_token,
            node_enabled: whatsapp.node_enabled,
            node_bind: whatsapp.node_bind,
            node_port: whatsapp.node_port,
            search: String::new(),
            notifications: true,
            high_contrast: false,
            copy_request: None,
            quit_requested: false,
            request_focus: true,
        }
    }

    fn send(&mut self, op: Op) {
        if let Err(error) = self.ops.try_send(op) {
            self.blocks.push(Block::Error(format!(
                "cannot send command to agent core: {error}"
            )));
        }
    }

    fn poll_whatsapp(&mut self, force: bool) {
        if self.whatsapp_request_pending
            || (!force && self.whatsapp_last_poll.elapsed() < Duration::from_secs(2))
        {
            return;
        }
        self.whatsapp_request_pending = true;
        self.whatsapp_last_poll = Instant::now();
        let sender = self.whatsapp_tx.clone();
        let url = format!("{}/api/whatsapp/status", self.whatsapp_service.api_base);
        let token = self.whatsapp_service.token.clone();
        self.runtime.spawn(async move {
            let result = http_json(
                reqwest::Client::new()
                    .get(url)
                    .header("X-Gnomef-Token", token),
            )
            .await;
            let _ = sender.send(WhatsAppReply::Status(result));
        });
    }

    fn poll_whatsapp_conversations(&mut self, force: bool) {
        if self.whatsapp_conversations_pending
            || (!force && self.whatsapp_conversations_last_poll.elapsed() < Duration::from_secs(2))
        {
            return;
        }
        self.whatsapp_conversations_pending = true;
        self.whatsapp_conversations_last_poll = Instant::now();
        let sender = self.whatsapp_tx.clone();
        let api_base = self.whatsapp_service.api_base.clone();
        let token = self.whatsapp_service.token.clone();
        let selected = self.whatsapp_selected_chat.clone();
        self.runtime.spawn(async move {
            let result = async {
                let list = http_json(
                    reqwest::Client::new()
                        .get(format!("{api_base}/api/chats"))
                        .header("X-Gnomef-Token", &token),
                )
                .await?;
                let chats = whatsapp_chat_summaries(&list);
                let active_id = selected
                    .filter(|selected| chats.iter().any(|(id, _)| id == selected))
                    .or_else(|| chats.first().map(|(id, _)| id.clone()));
                let active_chat = if let Some(id) = active_id.as_deref() {
                    Some(
                        http_json(
                            reqwest::Client::new()
                                .get(format!("{api_base}/api/chats/{id}"))
                                .header("X-Gnomef-Token", &token),
                        )
                        .await?,
                    )
                } else {
                    None
                };
                Ok(WhatsAppConversationSnapshot {
                    chats,
                    active_id,
                    active_chat,
                })
            }
            .await;
            let _ = sender.send(WhatsAppReply::Conversations(result));
        });
    }

    fn poll_nodes(&mut self, force: bool) {
        if !self.node_enabled
            || self.node_request_pending
            || (!force && self.node_last_poll.elapsed() < Duration::from_secs(3))
        {
            return;
        }
        self.node_request_pending = true;
        self.node_last_poll = Instant::now();
        let sender = self.node_tx.clone();
        let url = format!("{}/v1/nodes", self.node_api_base);
        let token = self.node_admin_token.clone();
        self.runtime.spawn(async move {
            let result = http_json(
                reqwest::Client::new()
                    .get(url)
                    .header("X-GnomeAI-Admin-Token", token),
            )
            .await;
            let _ = sender.send(NodeReply::List(result));
        });
    }

    fn set_node_policy(&mut self, node_id: String, policy: String) {
        self.node_feedback = Some("Updating root permission…".into());
        let sender = self.node_tx.clone();
        let url = format!("{}/v1/nodes/{node_id}/policy", self.node_api_base);
        let token = self.node_admin_token.clone();
        self.runtime.spawn(async move {
            let result = http_json(
                reqwest::Client::new()
                    .post(url)
                    .header("X-GnomeAI-Admin-Token", token)
                    .json(&serde_json::json!({"policy": policy})),
            )
            .await;
            let _ = sender.send(NodeReply::Policy(result));
        });
    }

    fn reload_whatsapp_service(&mut self) {
        self.whatsapp_feedback = Some("Applying WhatsApp settings…".into());
        let sender = self.whatsapp_tx.clone();
        let url = format!("{}/api/whatsapp/reload", self.whatsapp_service.api_base);
        let token = self.whatsapp_service.token.clone();
        self.runtime.spawn(async move {
            let result = http_json(
                reqwest::Client::new()
                    .post(url)
                    .header("X-Gnomef-Token", token),
            )
            .await;
            let _ = sender.send(WhatsAppReply::Action(result));
        });
    }

    fn refresh_whatsapp_qr(&mut self) {
        self.whatsapp_feedback = Some("Generating a new QR code…".into());
        let sender = self.whatsapp_tx.clone();
        let url = format!("{}/api/whatsapp/qr/refresh", self.whatsapp_service.api_base);
        let token = self.whatsapp_service.token.clone();
        self.runtime.spawn(async move {
            let result = http_json(
                reqwest::Client::new()
                    .post(url)
                    .header("X-Gnomef-Token", token),
            )
            .await;
            let _ = sender.send(WhatsAppReply::Action(result));
        });
    }

    fn send_whatsapp_test(&mut self) {
        let jid = self.whatsapp_test_jid.trim().to_string();
        let message = self.whatsapp_test_message.trim().to_string();
        if jid.is_empty() || message.is_empty() {
            self.whatsapp_feedback = Some("Enter the JID and test message.".into());
            return;
        }
        self.whatsapp_feedback = Some("Sending message…".into());
        let sender = self.whatsapp_tx.clone();
        let url = format!("{}/send", self.whatsapp_service.bridge_base);
        let token = self.whatsapp_service.token.clone();
        self.runtime.spawn(async move {
            let result = http_json(
                reqwest::Client::new()
                    .post(url)
                    .header("X-Gnomef-Token", token)
                    .json(&serde_json::json!({"jid": jid, "text": message})),
            )
            .await;
            let _ = sender.send(WhatsAppReply::Sent(result));
        });
    }

    fn drain_events(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            self.apply_event(event);
        }
        while let Ok(login) = self.login_rx.try_recv() {
            match login {
                LoginUpdate::DeviceCode {
                    verification_url,
                    user_code,
                } => {
                    let browser_error = open_external_url(&verification_url).err();
                    self.copy_request = Some(user_code.clone());
                    self.login_status =
                        Some("The OpenAI page was opened; the code was copied.".into());
                    self.device_login = Some(DeviceLoginDialog {
                        verification_url,
                        user_code,
                        browser_error,
                    });
                }
                LoginUpdate::Finished {
                    provider_id,
                    result,
                } => match result {
                    Ok(()) => {
                        self.device_login = None;
                        self.login_status =
                            Some("Authentication complete; activating provider…".into());
                        self.send(Op::SetProvider {
                            provider_id,
                            api_key: None,
                            base_url: None,
                        });
                    }
                    Err(error) => {
                        self.login_status = Some(format!("Authentication failed: {error}"));
                        self.blocks.push(Block::Error(error));
                    }
                },
            }
        }
        while let Ok(reply) = self.whatsapp_rx.try_recv() {
            match reply {
                WhatsAppReply::Status(Ok(status)) => {
                    self.whatsapp_status = status;
                    self.whatsapp_request_pending = false;
                }
                WhatsAppReply::Status(Err(error)) => {
                    self.whatsapp_request_pending = false;
                    self.whatsapp_feedback = Some(error);
                }
                WhatsAppReply::Action(Ok(value)) => {
                    self.whatsapp_feedback = Some(
                        value
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("WhatsApp settings were applied.")
                            .to_string(),
                    );
                    self.poll_whatsapp(true);
                }
                WhatsAppReply::Action(Err(error)) => {
                    self.whatsapp_feedback = Some(error);
                    self.poll_whatsapp(true);
                }
                WhatsAppReply::Sent(Ok(value)) => {
                    let queued = value
                        .get("queued")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    self.whatsapp_feedback = Some(if queued {
                        "The message was queued for WhatsApp.".into()
                    } else {
                        "The WhatsApp message was sent.".into()
                    });
                    self.whatsapp_test_message.clear();
                }
                WhatsAppReply::Sent(Err(error)) => self.whatsapp_feedback = Some(error),
                WhatsAppReply::Conversations(Ok(snapshot)) => {
                    self.whatsapp_conversations_pending = false;
                    self.whatsapp_conversations = snapshot.chats;
                    let selected_is_available =
                        self.whatsapp_selected_chat
                            .as_ref()
                            .is_some_and(|selected| {
                                self.whatsapp_conversations
                                    .iter()
                                    .any(|(id, _)| id == selected)
                            });
                    if !selected_is_available {
                        self.whatsapp_selected_chat = snapshot.active_id.clone();
                    }
                    if self.whatsapp_selected_chat == snapshot.active_id {
                        self.whatsapp_active_chat = snapshot.active_chat;
                    } else {
                        // The user selected another row while this request was in flight.
                        // Keep that selection and immediately fetch its transcript.
                        self.whatsapp_active_chat = None;
                        self.poll_whatsapp_conversations(true);
                    }
                }
                WhatsAppReply::Conversations(Err(error)) => {
                    self.whatsapp_conversations_pending = false;
                    self.whatsapp_feedback = Some(error);
                }
            }
        }
        while let Ok(reply) = self.node_rx.try_recv() {
            self.node_request_pending = false;
            match reply {
                NodeReply::List(Ok(value)) => {
                    self.node_status = value;
                    self.node_feedback = None;
                }
                NodeReply::List(Err(error)) => self.node_feedback = Some(error),
                NodeReply::Policy(Ok(_)) => {
                    self.node_feedback = Some("Root permission was updated.".into());
                    self.poll_nodes(true);
                }
                NodeReply::Policy(Err(error)) => self.node_feedback = Some(error),
            }
        }

        if !self.busy && self.approval.is_none() {
            if let Some(message) = self.queue.pop_front() {
                self.submit_message(message.text, message.attachment);
            }
        }
    }

    fn apply_event(&mut self, event: Event) {
        match event {
            Event::Ready {
                session_id,
                provider,
                model,
                workspace,
                sandbox,
                web_search_enabled,
                git_branch,
                recent_workspaces,
                models,
                mcp_servers,
            } => {
                self.session_id = session_id;
                self.provider = provider;
                self.model = model;
                self.workspace = workspace;
                self.sandbox = sandbox;
                self.web_search_enabled = web_search_enabled;
                self.branch = git_branch;
                self.recent_workspaces = recent_workspaces;
                self.mcp_servers = mcp_servers;
                self.set_models(models);
                self.send(Op::ListSessions);
            }
            Event::SessionReset => {
                self.blocks.clear();
                self.queue.clear();
                self.busy = false;
                self.tokens_in = 0;
                self.tokens_out = 0;
                self.token_history.clear();
                self.send(Op::ListSessions);
            }
            Event::SessionList { sessions } => {
                self.sessions = sessions;
                if self
                    .confirm_delete_session
                    .as_ref()
                    .is_some_and(|id| !self.sessions.iter().any(|session| &session.id == id))
                {
                    self.confirm_delete_session = None;
                }
            }
            Event::HistoryReplay { turns } => {
                for turn in turns {
                    match turn.role.as_str() {
                        "user" => self.blocks.push(Block::User(turn.text)),
                        "assistant" => self.blocks.push(Block::Assistant(turn.text)),
                        _ => self.blocks.push(Block::Note(turn.text)),
                    }
                }
            }
            Event::ProviderChanged {
                provider,
                model,
                models,
            } => {
                self.provider = provider;
                self.model = model;
                self.set_models(models);
                self.provider_api_key.clear();
                self.show_provider = false;
            }
            Event::WebSearchChanged { enabled } => self.web_search_enabled = enabled,
            Event::McpConfigChanged { servers } => self.mcp_servers = servers,
            Event::WhatsAppConfigChanged {
                enabled,
                assistant_name,
                has_own_number,
                allowed_jids,
            } => {
                self.whatsapp_enabled = enabled;
                self.whatsapp_assistant_name = assistant_name;
                self.whatsapp_has_own_number = has_own_number;
                self.whatsapp_allowed_jids = allowed_jids.join("\n");
                self.reload_whatsapp_service();
            }
            Event::NodeHubConfigChanged {
                enabled,
                bind,
                port,
            } => {
                self.node_enabled = enabled;
                self.node_bind = bind;
                self.node_port = port;
                self.node_api_base = format!("http://127.0.0.1:{port}");
                self.node_feedback =
                    Some("Settings were saved. Restart GnomeAI to apply the listener.".into());
            }
            Event::TurnStarted { .. } => {
                self.busy = true;
                self.started = Some(Instant::now());
            }
            Event::Token { text } => match self.blocks.last_mut() {
                Some(Block::Assistant(buffer)) => buffer.push_str(&text),
                _ => self.blocks.push(Block::Assistant(text)),
            },
            Event::Reasoning { text } => match self.blocks.last_mut() {
                Some(Block::Reasoning(buffer)) => buffer.push_str(&text),
                _ => self.blocks.push(Block::Reasoning(text)),
            },
            Event::ToolCallStarted {
                call_id,
                name,
                summary,
            } => self.blocks.push(Block::Tool {
                call_id,
                name,
                summary,
                output: String::new(),
                done: false,
                ok: false,
                ms: 0,
            }),
            Event::ToolOutput { call_id, chunk } => {
                if let Some(Block::Tool { output, .. }) = self.blocks.iter_mut().rev().find(
                    |block| matches!(block, Block::Tool { call_id: id, .. } if id == &call_id),
                ) {
                    output.push_str(&chunk);
                    if output.len() > 16_384 {
                        let mut cut = output.len() - 16_384;
                        while !output.is_char_boundary(cut) {
                            cut += 1;
                        }
                        *output = output[cut..].to_string();
                    }
                }
            }
            Event::ToolCallEnded {
                call_id,
                ok,
                duration_ms,
            } => {
                if let Some(Block::Tool {
                    done,
                    ok: success,
                    ms,
                    ..
                }) = self.blocks.iter_mut().rev().find(
                    |block| matches!(block, Block::Tool { call_id: id, .. } if id == &call_id),
                ) {
                    *done = true;
                    *success = ok;
                    *ms = duration_ms;
                }
            }
            Event::ApprovalRequest {
                call_id,
                command,
                reason,
                allow_always,
                ..
            } => {
                if self.notifications {
                    desktop_notify("GnomeAI — approval needed", &command);
                }
                self.approval = Some(ApprovalDialog {
                    call_id,
                    command,
                    reason,
                    allow_always,
                });
            }
            Event::PrivilegeCredentialRequest {
                request_id,
                command,
                keyring_available,
                attempt,
                prompt,
                dynamic,
                message,
            } => {
                self.privilege = Some(PrivilegeDialog {
                    request_id,
                    command,
                    credential: String::new(),
                    remember: false,
                    keyring_available,
                    attempt,
                    prompt,
                    dynamic,
                    message,
                });
            }
            Event::PatchApplied { diff, .. } => self.blocks.push(Block::Diff(diff)),
            Event::Verification {
                stage,
                passed,
                summary,
            } => self.blocks.push(Block::Verify {
                stage,
                passed,
                summary,
            }),
            Event::Compacted { freed_tokens } => self.blocks.push(Block::Note(format!(
                "Context compacted; {freed_tokens} tokens freed."
            ))),
            Event::TurnCompleted {
                input_tokens,
                output_tokens,
                duration_ms,
                ..
            } => {
                self.busy = false;
                self.started = None;
                self.tokens_in += input_tokens;
                self.tokens_out += output_tokens;
                self.token_history
                    .push((input_tokens, output_tokens, duration_ms));
                if self.notifications {
                    desktop_notify(
                        "GnomeAI — turn complete",
                        &format!(
                            "{output_tokens} tokens out · {:.1}s · {}",
                            duration_ms as f64 / 1000.0,
                            self.model
                        ),
                    );
                }
            }
            Event::Interrupted => {
                self.busy = false;
                self.started = None;
                self.blocks.push(Block::Note("Interrupted.".into()));
            }
            Event::Notice { message } => self.blocks.push(Block::Note(message)),
            Event::Error { message, fatal } => {
                self.busy = false;
                self.started = None;
                self.fatal = fatal;
                self.blocks.push(Block::Error(message));
            }
        }
    }

    fn set_models(&mut self, mut models: Vec<String>) {
        models.retain(|model| !model.trim().is_empty());
        models.sort_unstable();
        models.dedup();
        if !self.model.trim().is_empty() && !models.contains(&self.model) {
            models.insert(0, self.model.clone());
        }
        self.models = models;
    }

    fn dispatch_composer(&mut self) {
        let text = std::mem::take(&mut self.composer);
        let attachment = self.pending_attachment.take();
        if text.trim().is_empty() && attachment.is_none() {
            return;
        }
        self.history.push(text.clone());
        self.history_pos = None;
        if self.busy {
            self.queue.push_back(QueuedMessage { text, attachment });
            self.blocks.push(Block::Note(format!(
                "Message queued ({} waiting).",
                self.queue.len()
            )));
        } else {
            self.submit_message(text, attachment);
        }
        self.request_focus = true;
    }

    fn submit_message(&mut self, text: String, attachment: Option<PathBuf>) {
        if attachment.is_none() && self.handle_command(text.trim()) {
            return;
        }
        if attachment.is_none() {
            if let Some(path) = workspace_path_from_message(text.trim()) {
                self.send(Op::SetWorkspace { path });
                return;
            }
        }

        let mut model_text = text.clone();
        let mut display_text = text;
        if let Some(path) = attachment {
            match encode_attachment(&path, &model_text) {
                Ok(encoded) => {
                    model_text = encoded;
                    let name = path
                        .file_name()
                        .map(|name| name.to_string_lossy())
                        .unwrap_or_else(|| path.as_os_str().to_string_lossy());
                    if !display_text.is_empty() {
                        display_text.push('\n');
                    }
                    display_text.push_str(&format!("📎 {name}"));
                }
                Err(error) => {
                    self.blocks.push(Block::Error(error));
                    return;
                }
            }
        }
        self.blocks.push(Block::User(display_text));
        self.send(Op::Submit { text: model_text });
    }

    fn handle_command(&mut self, text: &str) -> bool {
        match text {
            "/help" | "/?" | "/commands" => self.show_help = true,
            "/new" => self.send(Op::NewSession),
            "/sessions" => self.send(Op::ListSessions),
            "/resume" => self
                .blocks
                .push(Block::Note("Use /resume ID or the Sessions window.".into())),
            "/fork" => self.send(Op::ForkSession),
            "/compact" => self.send(Op::Compact),
            "/rollback" => self.send(Op::Rollback),
            "/workspace" | "/cd" => self.choose_workspace(),
            "/provider" => self.show_provider = true,
            "/model" => self.show_models = true,
            "/websearch" => self.send(Op::SetWebSearch {
                enabled: !self.web_search_enabled,
            }),
            "/whatsapp" => {
                self.show_whatsapp_conversations = true;
                self.poll_whatsapp(true);
                self.poll_whatsapp_conversations(true);
            }
            "/nodes" => {
                self.show_nodes = true;
                self.poll_nodes(true);
            }
            "/sandbox" => self.blocks.push(Block::Note(
                "Use the Sandbox selector in the sidebar or /sandbox MODE.".into(),
            )),
            "/skills" => self.send(Op::SkillsList),
            "/skill" => self.blocks.push(Block::Note(
                "Use /skill use|inspect|install|update|verify|remove ARG.".into(),
            )),
            "/memory" => self.send(Op::MemoryShow),
            "/copy" => {
                if let Some(text) = self.blocks.iter().rev().find_map(|block| match block {
                    Block::Assistant(text) if !text.is_empty() => Some(text.clone()),
                    _ => None,
                }) {
                    self.copy_request = Some(text);
                    self.blocks
                        .push(Block::Note("The last assistant reply was copied.".into()));
                } else {
                    self.blocks.push(Block::Note(
                        "There is no assistant reply to copy yet.".into(),
                    ));
                }
            }
            "/contrast" => self.high_contrast = !self.high_contrast,
            "/notify" => self.notifications = !self.notifications,
            "/mouse" => self.blocks.push(Block::Note(
                "Mouse input is always enabled in the graphical interface.".into(),
            )),
            "/tokens" => self.show_token_usage(),
            "/doctor" => self.send(Op::Doctor),
            "/diff" => self.send(Op::ShowDiff),
            "/export" => self.export_conversation(),
            "/clear" => {
                self.blocks.clear();
                self.blocks.push(Block::Note(
                    "Transcript cleared; session history is preserved.".into(),
                ));
            }
            "/quit" => self.quit_requested = true,
            _ if text.starts_with("/workspace ") || text.starts_with("/cd ") => {
                let path = text
                    .split_once(' ')
                    .map(|(_, path)| path.trim())
                    .unwrap_or("");
                if path.is_empty() {
                    self.blocks
                        .push(Block::Error("usage: /workspace PATH".into()));
                } else {
                    self.send(Op::SetWorkspace {
                        path: PathBuf::from(unquote(path)),
                    });
                }
            }
            _ if text.starts_with("/resume ") => {
                let id = text.trim_start_matches("/resume ").trim();
                self.send(Op::ResumeSession { id: id.into() });
            }
            _ if text.starts_with("/model ") => {
                let model = text.trim_start_matches("/model ").trim();
                if model.is_empty() {
                    self.blocks.push(Block::Error("usage: /model MODEL".into()));
                } else {
                    self.send(Op::SetModel {
                        model: model.into(),
                    });
                }
            }
            _ if text.starts_with("/sandbox ") => {
                let mode = text.trim_start_matches("/sandbox ").trim();
                self.send(Op::SetSandbox { mode: mode.into() });
            }
            _ if text.starts_with("/websearch ") => {
                let value = text.trim_start_matches("/websearch ").trim();
                match value {
                    "on" | "true" | "1" => self.send(Op::SetWebSearch { enabled: true }),
                    "off" | "false" | "0" => self.send(Op::SetWebSearch { enabled: false }),
                    _ => self
                        .blocks
                        .push(Block::Error("usage: /websearch on|off".into())),
                }
            }
            _ if text.starts_with("/notify ") => {
                let value = text.trim_start_matches("/notify ").trim();
                match value {
                    "on" | "true" | "1" => self.notifications = true,
                    "off" | "false" | "0" => self.notifications = false,
                    _ => self
                        .blocks
                        .push(Block::Error("usage: /notify on|off".into())),
                }
            }
            _ if text.starts_with("/memory ") => self.handle_memory_command(text),
            _ if text.starts_with("/skill ") => self.handle_skill_command(text),
            _ => return false,
        }
        true
    }

    fn handle_memory_command(&mut self, text: &str) {
        let value = text.trim_start_matches("/memory ").trim();
        match value {
            "show" | "list" => self.send(Op::MemoryShow),
            "status" => self.send(Op::MemoryStatus),
            "dream" => self.send(Op::MemoryDream { dry_run: false }),
            "dream --dry-run" | "dream dry-run" => self.send(Op::MemoryDream { dry_run: true }),
            "reindex" => self.send(Op::MemoryReindex),
            "clear" | "wipe" => self.send(Op::MemoryClear),
            "on" => self.send(Op::MemorySet { enabled: true }),
            "off" => self.send(Op::MemorySet { enabled: false }),
            _ if value.starts_with("forget ") => self.send(Op::MemoryForget {
                id: value.trim_start_matches("forget ").trim().into(),
            }),
            _ => self.blocks.push(Block::Error(
                "usage: /memory status|show|dream [--dry-run]|reindex|forget ID|clear|on|off"
                    .into(),
            )),
        }
    }

    fn handle_skill_command(&mut self, text: &str) {
        let value = text.trim_start_matches("/skill ").trim();
        let (action, argument) = value.split_once(' ').unwrap_or((value, ""));
        if argument.trim().is_empty() {
            self.blocks.push(Block::Error(
                "usage: /skill use|inspect|install|update|verify|remove ARG".into(),
            ));
            return;
        }
        let argument = argument.trim().to_string();
        let op = match action {
            "use" | "activate" => Op::SkillActivate { name: argument },
            "show" | "inspect" => Op::SkillInspect { name: argument },
            "install" => Op::SkillInstall { source: argument },
            "update" => Op::SkillUpdate { name: argument },
            "verify" => Op::SkillVerify { name: argument },
            "remove" | "uninstall" => Op::SkillRemove { name: argument },
            _ => {
                self.blocks.push(Block::Error(
                    "usage: /skill use|inspect|install|update|verify|remove ARG".into(),
                ));
                return;
            }
        };
        self.send(op);
    }

    fn choose_workspace(&mut self) {
        let mut dialog = rfd::FileDialog::new().set_title("Choose GnomeAI workspace");
        if self.workspace.is_dir() {
            dialog = dialog.set_directory(&self.workspace);
        }
        if let Some(path) = dialog.pick_folder() {
            self.send(Op::SetWorkspace { path });
        }
    }

    fn choose_skill_file(&mut self) {
        let mut dialog = rfd::FileDialog::new()
            .set_title("Install SKILL.md")
            .add_filter("Agent Skill", &["md"]);
        if self.workspace.is_dir() {
            dialog = dialog.set_directory(&self.workspace);
        }
        let Some(path) = dialog.pick_file() else {
            return;
        };
        if path.file_name().and_then(|name| name.to_str()) != Some("SKILL.md") {
            self.blocks
                .push(Block::Error("Select a file named exactly SKILL.md.".into()));
            return;
        }
        let Some(directory) = path.parent() else {
            self.blocks.push(Block::Error(
                "SKILL.md does not have a valid parent directory.".into(),
            ));
            return;
        };
        self.send(Op::SkillInstall {
            source: directory.display().to_string(),
        });
    }

    fn choose_attachment(&mut self) {
        let mut supported = Vec::from(IMAGE_EXTS);
        supported.push("pdf");
        supported.extend_from_slice(OOXML_EXTS);
        supported.extend_from_slice(TEXT_EXTS);
        let mut dialog = rfd::FileDialog::new()
            .set_title("Attach a file")
            .add_filter("Supported files", &supported)
            .add_filter("Documents", &["pdf", "docx", "xlsx", "pptx"])
            .add_filter("Code and text", TEXT_EXTS)
            .add_filter("Images", IMAGE_EXTS);
        if self.workspace.is_dir() {
            dialog = dialog.set_directory(&self.workspace);
        }
        if let Some(path) = dialog.pick_file() {
            self.pending_attachment = Some(path);
            self.request_focus = true;
        }
    }

    fn handle_dropped_files(&mut self, ctx: &egui::Context) {
        let dropped = ctx.input(|input| input.raw.dropped_files.clone());
        let Some(file) = dropped.into_iter().next() else {
            return;
        };
        let path = file.path().to_path_buf();
        if path.as_os_str().is_empty() {
            self.blocks.push(Block::Error(
                "The dropped file does not provide a local path; use the + button to select it."
                    .into(),
            ));
            return;
        }
        self.pending_attachment = Some(path);
        self.request_focus = true;
    }

    fn export_conversation(&mut self) {
        let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
        let filename = format!("gnomeai_export_{timestamp}.md");
        let mut dialog = rfd::FileDialog::new()
            .set_title("Export conversation")
            .set_file_name(filename)
            .add_filter("Markdown", &["md"]);
        if self.workspace.is_dir() {
            dialog = dialog.set_directory(&self.workspace);
        }
        let Some(path) = dialog.save_file() else {
            return;
        };
        match std::fs::write(&path, self.export_markdown()) {
            Ok(()) => self
                .blocks
                .push(Block::Note(format!("Exported to {}", path.display()))),
            Err(error) => self.blocks.push(Block::Error(format!(
                "Cannot export to {}: {error}",
                path.display()
            ))),
        }
    }

    fn export_markdown(&self) -> String {
        let mut output = format!(
            "# GnomeAI-RS conversation\n\n**Provider:** {}  \n**Model:** {}  \n**Workspace:** {}\n\n---\n\n",
            self.provider,
            self.model,
            self.workspace.display()
        );
        for block in &self.blocks {
            match block {
                Block::User(text) => output.push_str(&format!("## User\n\n{text}\n\n")),
                Block::Assistant(text) => output.push_str(&format!("## Assistant\n\n{text}\n\n")),
                Block::Reasoning(text) => output.push_str(&format!(
                    "<details><summary>Reasoning</summary>\n\n{text}\n\n</details>\n\n"
                )),
                Block::Tool {
                    name,
                    summary,
                    output: tool_output,
                    ok,
                    ..
                } => output.push_str(&format!(
                    "### Tool: {name} ({})\n\n{summary}\n\n```text\n{tool_output}\n```\n\n",
                    if *ok { "ok" } else { "failed" }
                )),
                Block::Diff(diff) => output.push_str(&format!("```diff\n{diff}\n```\n\n")),
                Block::Verify {
                    stage,
                    passed,
                    summary,
                } => output.push_str(&format!(
                    "**Verification {stage}: {}** — {summary}\n\n",
                    if *passed { "passed" } else { "failed" }
                )),
                Block::Error(error) => output.push_str(&format!("> Error: {error}\n\n")),
                Block::Note(note) => output.push_str(&format!("> {note}\n\n")),
            }
        }
        output
    }

    fn show_token_usage(&mut self) {
        if self.token_history.is_empty() {
            self.blocks
                .push(Block::Note("No completed turns yet.".into()));
            return;
        }
        let mut text = String::from("turn | in | out | total | duration\n");
        for (index, (input, output, ms)) in self.token_history.iter().enumerate() {
            text.push_str(&format!(
                "{} | {} | {} | {} | {:.1}s\n",
                index + 1,
                input,
                output,
                input + output,
                *ms as f64 / 1000.0
            ));
        }
        text.push_str(&format!(
            "\nTotal: {} in / {} out · model {}",
            self.tokens_in, self.tokens_out, self.model
        ));
        self.blocks.push(Block::Note(text));
    }

    fn spawn_login(&mut self, provider_id: String) {
        let sender = self.login_tx.clone();
        let login_id = provider_id.clone();
        self.login_status = Some("Starting authentication in your browser…".into());
        self.device_login = None;
        self.show_provider = false;
        self.runtime.spawn(async move {
            let result = match login_id.as_str() {
                "openai-account" => {
                    let progress = sender.clone();
                    crate::codex_app_server::login_with_chatgpt_notifying(
                        move |verification_url, user_code| {
                            let _ = progress.send(LoginUpdate::DeviceCode {
                                verification_url,
                                user_code,
                            });
                        },
                    )
                    .await
                }
                "anthropic-account" => crate::provider::login_with_claude().await,
                _ => Err(anyhow!("account login is not configured")),
            }
            .map_err(|error| error.to_string());
            let _ = sender.send(LoginUpdate::Finished {
                provider_id: login_id,
                result,
            });
        });
    }

    fn sidebar(&mut self, root: &mut egui::Ui) {
        let session_rows = self
            .sessions
            .iter()
            .take(8)
            .map(|session| {
                (
                    session.id.clone(),
                    session
                        .title
                        .clone()
                        .unwrap_or_else(|| "Untitled conversation".into()),
                    session
                        .workspace
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("project")
                        .to_string(),
                    session.is_current,
                )
            })
            .collect::<Vec<_>>();
        let workspace_name = self
            .workspace
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("Choose project")
            .to_string();
        let whatsapp_connected = whatsapp_bool(&self.whatsapp_status, "connected");
        let confirming_delete = self.confirm_delete_session.clone();
        let mut session_action = None;
        let sidebar_width = if root.available_width() < 1040.0 {
            228.0
        } else {
            264.0
        };

        egui::Panel::left("sidebar")
            .exact_size(sidebar_width)
            .resizable(false)
            .show_separator_line(false)
            .frame(
                egui::Frame::default()
                    .fill(Color32::from_rgb(22, 23, 26))
                    .inner_margin(Margin::symmetric(13, 15)),
            )
            .show(root, |ui| {
                ui.horizontal(|ui| {
                    let (logo, _) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::hover());
                    ui.painter()
                        .rect_filled(logo, 8.0, Color32::from_rgb(105, 205, 153));
                    ui.painter().text(
                        logo.center(),
                        egui::Align2::CENTER_CENTER,
                        "G",
                        FontId::new(16.0, FontFamily::Proportional),
                        Color32::from_rgb(18, 31, 24),
                    );
                    ui.vertical(|ui| {
                        ui.label(RichText::new("GnomeAI-RS").size(16.0).strong());
                        ui.label(
                            RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                                .small()
                                .color(Color32::from_rgb(125, 125, 132)),
                        );
                    });
                });
                ui.add_space(18.0);

                if new_conversation_button(ui, "New conversation").clicked() {
                    self.send(Op::NewSession);
                }
                ui.add_space(18.0);
                ui.label(
                    RichText::new("CONVERSATIONS")
                        .size(10.0)
                        .strong()
                        .color(Color32::from_rgb(120, 120, 128)),
                );
                ui.add_space(5.0);

                let list_height = (ui.available_height() - 238.0).max(120.0);
                ScrollArea::vertical()
                    .id_salt("sidebar_sessions")
                    .max_height(list_height)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if session_rows.is_empty() {
                            ui.label(
                                RichText::new("Recent conversations will appear here.")
                                    .small()
                                    .color(Color32::from_rgb(118, 118, 126)),
                            );
                        }
                        for (id, title, project, current) in &session_rows {
                            let action = session_row(
                                ui,
                                id,
                                title,
                                project,
                                *current,
                                confirming_delete.as_deref() == Some(id),
                            );
                            if action != SessionRowAction::None {
                                session_action = Some((id.clone(), action, *current));
                            }
                        }
                    });

                if nav_button(ui, "All conversations", false, NavIcon::More).clicked() {
                    self.show_sessions = true;
                    self.send(Op::ListSessions);
                }

                ui.add_space(8.0);
                ui.separator();
                ui.add_space(5.0);

                if nav_button(ui, &workspace_name, false, NavIcon::Folder).clicked() {
                    self.choose_workspace();
                }
                if nav_button(ui, &self.model, false, NavIcon::Model).clicked() {
                    self.show_models = true;
                }
                if nav_button(ui, "WhatsApp", whatsapp_connected, NavIcon::WhatsApp).clicked() {
                    self.show_whatsapp_conversations = true;
                    self.poll_whatsapp(true);
                    self.poll_whatsapp_conversations(true);
                }
                if nav_button(ui, "Settings", self.show_settings, NavIcon::Settings).clicked() {
                    self.show_settings = true;
                }
            });

        if let Some((id, action, current)) = session_action {
            match action {
                SessionRowAction::Resume if !current => {
                    self.confirm_delete_session = None;
                    self.send(Op::ResumeSession { id });
                }
                SessionRowAction::AskDelete => self.confirm_delete_session = Some(id),
                SessionRowAction::ConfirmDelete => {
                    self.confirm_delete_session = None;
                    self.send(Op::DeleteSession { id });
                }
                SessionRowAction::CancelDelete => self.confirm_delete_session = None,
                SessionRowAction::None | SessionRowAction::Resume => {}
            }
        }
    }

    fn status_bar(&mut self, root: &mut egui::Ui) {
        let conversation_title = self
            .sessions
            .iter()
            .find(|session| session.is_current)
            .and_then(|session| session.title.clone())
            .or_else(|| {
                self.workspace
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(|name| format!("Working in {name}"))
            })
            .unwrap_or_else(|| "New conversation".into());
        egui::Panel::top("status")
            .exact_size(64.0)
            .show_separator_line(true)
            .frame(
                egui::Frame::default()
                    .fill(Color32::from_rgb(20, 21, 24))
                    .inner_margin(Margin::symmetric(20, 9)),
            )
            .show(root, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(RichText::new(conversation_title).size(15.0).strong());
                        let mut context = self.workspace.display().to_string();
                        if let Some(branch) = &self.branch {
                            context.push_str(&format!("  ·  {branch}"));
                        }
                        ui.label(
                            RichText::new(ellipsize(&context, 72))
                                .small()
                                .color(Color32::from_rgb(128, 128, 136)),
                        );
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let (status, status_color) = if self.busy {
                            let elapsed = self
                                .started
                                .map_or(0.0, |time| time.elapsed().as_secs_f32());
                            (
                                format!("Working · {elapsed:.1}s"),
                                Color32::from_rgb(230, 183, 88),
                            )
                        } else if self.fatal {
                            ("Core stopped".into(), Color32::from_rgb(241, 112, 127))
                        } else {
                            ("Ready".into(), Color32::from_rgb(105, 205, 153))
                        };
                        status_indicator(ui, &status, status_color);
                        if ui
                            .selectable_label(self.show_activity, "Changes")
                            .on_hover_text("Show activity and diffs")
                            .clicked()
                        {
                            self.show_activity = !self.show_activity;
                        }
                        if !self.queue.is_empty() {
                            ui.label(
                                RichText::new(format!("{} queued", self.queue.len()))
                                    .small()
                                    .color(Color32::from_rgb(162, 163, 171)),
                            );
                        }
                        if ui
                            .small_button(ellipsize(
                                &format!("{} · {}", self.provider, self.model),
                                34,
                            ))
                            .on_hover_text("Change model")
                            .clicked()
                        {
                            self.show_models = true;
                        }
                    });
                });
            });
    }

    fn activity_panel(&mut self, root: &mut egui::Ui) {
        if !self.show_activity {
            return;
        }

        let latest_diff = self.blocks.iter().rev().find_map(|block| match block {
            Block::Diff(diff) => Some(diff.clone()),
            _ => None,
        });
        let file_count = latest_diff
            .as_deref()
            .map(diff_file_names)
            .map_or(0, |files| files.len());
        let activities = self
            .blocks
            .iter()
            .rev()
            .filter_map(|block| match block {
                Block::Tool {
                    name,
                    summary,
                    done,
                    ok,
                    ms,
                    ..
                } => Some((
                    format!(
                        "{} {name} · {summary}{}",
                        if !*done {
                            "●"
                        } else if *ok {
                            "✓"
                        } else {
                            "✕"
                        },
                        if *done {
                            format!(" · {ms} ms")
                        } else {
                            String::new()
                        }
                    ),
                    if !*done {
                        Color32::YELLOW
                    } else if *ok {
                        Color32::from_rgb(80, 200, 130)
                    } else {
                        Color32::LIGHT_RED
                    },
                )),
                Block::Verify {
                    stage,
                    passed,
                    summary,
                } => Some((
                    format!("{} {stage} · {summary}", if *passed { "✓" } else { "✕" }),
                    if *passed {
                        Color32::from_rgb(80, 200, 130)
                    } else {
                        Color32::LIGHT_RED
                    },
                )),
                Block::Error(error) => Some((format!("✕ {error}"), Color32::LIGHT_RED)),
                _ => None,
            })
            .take(8)
            .collect::<Vec<_>>();
        let panel_width = (root.available_width() * 0.34).clamp(280.0, 360.0);

        egui::Panel::right("activity")
            .exact_size(panel_width)
            .resizable(false)
            .show_separator_line(true)
            .frame(
                egui::Frame::default()
                    .fill(Color32::from_rgb(22, 23, 26))
                    .inner_margin(Margin::symmetric(15, 14)),
            )
            .show(root, |ui| {
                ui.horizontal(|ui| {
                    ui.heading(RichText::new(format!("Changes ({file_count})")).size(16.0));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.small_button("Close").clicked() {
                            self.show_activity = false;
                        }
                    });
                });
                ui.add_space(5.0);
                ui.horizontal(|ui| {
                    if ui.small_button("Refresh diff").clicked() {
                        self.send(Op::ShowDiff);
                    }
                    ui.menu_button("Actions", |ui| {
                        if ui.button("Skills").clicked() {
                            self.send(Op::SkillsList);
                            ui.close();
                        }
                        if ui.button("Memory").clicked() {
                            self.send(Op::MemoryShow);
                            ui.close();
                        }
                        if ui.button("Diagnostics").clicked() {
                            self.send(Op::Doctor);
                            ui.close();
                        }
                    });
                });
                ui.add_space(8.0);
                ui.separator();

                ScrollArea::vertical()
                    .id_salt("activity_scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new("FILES")
                                .size(10.0)
                                .strong()
                                .color(Color32::from_rgb(120, 120, 128)),
                        );
                        ui.add_space(5.0);
                        match latest_diff.as_deref() {
                            Some(diff) => {
                                let files = diff_file_names(diff);
                                if files.is_empty() {
                                    ui.label(
                                        RichText::new("There are changes in this session.")
                                            .small()
                                            .color(Color32::GRAY),
                                    );
                                } else {
                                    for file in files {
                                        egui::Frame::default()
                                            .fill(Color32::from_rgb(29, 29, 33))
                                            .corner_radius(7.0)
                                            .inner_margin(Margin::symmetric(8, 6))
                                            .show(ui, |ui| {
                                                ui.label(RichText::new(file).monospace().small());
                                            });
                                        ui.add_space(3.0);
                                    }
                                }

                                ui.add_space(10.0);
                                ui.label(
                                    RichText::new("DIFF PREVIEW")
                                        .size(10.0)
                                        .strong()
                                        .color(Color32::from_rgb(120, 120, 128)),
                                );
                                egui::Frame::default()
                                    .fill(Color32::from_rgb(14, 14, 16))
                                    .corner_radius(8.0)
                                    .inner_margin(8.0)
                                    .show(ui, |ui| {
                                        ScrollArea::horizontal().show(ui, |ui| {
                                            for line in diff.lines().take(80) {
                                                let color = diff_line_color(line);
                                                ui.label(
                                                    RichText::new(line)
                                                        .monospace()
                                                        .small()
                                                        .color(color),
                                                );
                                            }
                                        });
                                    });
                            }
                            None => {
                                ui.label(
                                    RichText::new("There are no changes in this session yet.")
                                        .small()
                                        .color(Color32::GRAY),
                                );
                            }
                        }

                        ui.add_space(14.0);
                        ui.label(
                            RichText::new("ACTIVITY")
                                .size(10.0)
                                .strong()
                                .color(Color32::from_rgb(120, 120, 128)),
                        );
                        ui.add_space(5.0);
                        if activities.is_empty() {
                            ui.label(
                                RichText::new(
                                    "Commands and verification results will appear here.",
                                )
                                .small()
                                .color(Color32::GRAY),
                            );
                        }
                        for (text, color) in activities {
                            ui.add(
                                egui::Label::new(RichText::new(text).small().color(color)).wrap(),
                            );
                            ui.add_space(4.0);
                        }
                    });
            });
    }

    fn transcript(&mut self, root: &mut egui::Ui) {
        egui::CentralPanel::default()
            .frame(
                egui::Frame::default()
                    .fill(Color32::from_rgb(15, 15, 17))
                    .inner_margin(Margin::symmetric(18, 14)),
            )
            .show(root, |ui| {
                ScrollArea::vertical()
                    .id_salt("transcript")
                    .auto_shrink([false, false])
                    .stick_to_bottom(true)
                    // TextEdit owns the pointer while text is being selected.
                    // In egui 0.36 that prevents ScrollArea's built-in wheel
                    // path from running (`dragged_id` is set). Disable that
                    // path and forward the wheel explicitly from the content
                    // UI so scrolling keeps working over text and mid-drag.
                    .wheel_scroll_multiplier(Vec2::ZERO)
                    .show(ui, |ui| {
                        forward_mouse_wheel_to_parent_scroll(ui);
                        let content_width = ui.available_width().min(900.0);
                        let gutter = ((ui.available_width() - content_width) / 2.0).max(0.0);
                        ui.horizontal(|ui| {
                            ui.add_space(gutter);
                            ui.vertical(|ui| {
                                ui.set_width(content_width);
                                if self.blocks.is_empty() {
                                    ui.vertical_centered(|ui| {
                                        ui.add_space(54.0);
                                        let (logo, _) = ui
                                            .allocate_exact_size(Vec2::splat(46.0), Sense::hover());
                                        ui.painter().circle_filled(
                                            logo.center(),
                                            23.0,
                                            Color32::from_rgb(105, 205, 153),
                                        );
                                        ui.painter().text(
                                            logo.center(),
                                            egui::Align2::CENTER_CENTER,
                                            "G",
                                            FontId::new(23.0, FontFamily::Proportional),
                                            Color32::from_rgb(18, 31, 24),
                                        );
                                        ui.add_space(12.0);
                                        ui.heading(
                                            RichText::new("What are we working on today?").size(25.0),
                                        );
                                        ui.label(
                                            RichText::new(
                                                "Ask for a change, attach an image, or choose a quick action.",
                                            )
                                            .color(Color32::from_rgb(151, 152, 161)),
                                        );
                                        ui.add_space(22.0);

                                        if suggestion_button(
                                            ui,
                                            "Analyze the current project",
                                            "Structure, issues, and improvements",
                                        )
                                        .clicked()
                                        {
                                            self.composer = "Analyze the current project and tell me what should be improved.".into();
                                            self.request_focus = true;
                                        }
                                        if suggestion_button(
                                            ui,
                                            "Show Git changes",
                                            "Changed files and diff",
                                        )
                                        .clicked()
                                        {
                                            self.show_activity = true;
                                            self.send(Op::ShowDiff);
                                        }
                                        if suggestion_button(
                                            ui,
                                            "Configure WhatsApp",
                                            "Connection, QR, and allowed numbers",
                                        )
                                        .clicked()
                                        {
                                            self.show_whatsapp = true;
                                            self.poll_whatsapp(true);
                                        }
                                    });
                                }
                                let filter = self.search.trim().to_lowercase();
                                for (index, block) in self.blocks.iter().enumerate() {
                                    if !filter.is_empty()
                                        && !block
                                            .searchable_text()
                                            .to_lowercase()
                                            .contains(&filter)
                                    {
                                        continue;
                                    }
                                    render_block(ui, index, block);
                                    ui.add_space(10.0);
                                }
                            });
                        });
                    });
            });
    }

    fn composer(&mut self, root: &mut egui::Ui) {
        let ctx = root.ctx().clone();
        egui::Panel::bottom("composer")
            .show_separator_line(false)
            .frame(
                egui::Frame::default()
                    .fill(Color32::from_rgb(17, 18, 20))
                    .inner_margin(Margin::symmetric(18, 12)),
            )
            .show(root, |ui| {
                let content_width = ui.available_width().min(900.0);
                let gutter = ((ui.available_width() - content_width) / 2.0).max(0.0);
                ui.horizontal(|ui| {
                    ui.add_space(gutter);
                    ui.vertical(|ui| {
                        ui.set_width(content_width);

                        if let Some(path) = self.pending_attachment.clone() {
                            egui::Frame::default()
                                .fill(Color32::from_rgb(27, 28, 32))
                                .corner_radius(8.0)
                                .inner_margin(Margin::symmetric(9, 5))
                                .show(ui, |ui| {
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            RichText::new(format!("File · {}", path.display()))
                                                .small()
                                                .color(Color32::from_rgb(183, 184, 191)),
                                        );
                                        if ui.small_button("Remove").clicked() {
                                            self.pending_attachment = None;
                                        }
                                    });
                                });
                            ui.add_space(5.0);
                        }

                        let prefix = self.composer.trim_start();
                        if prefix.starts_with('/') && !prefix.contains(char::is_whitespace) {
                            let matches = COMMANDS
                                .iter()
                                .filter(|(command, _)| command.starts_with(prefix))
                                .take(7)
                                .copied()
                                .collect::<Vec<_>>();
                            if !matches.is_empty() {
                                egui::Frame::default()
                                    .fill(Color32::from_rgb(25, 26, 30))
                                    .stroke(Stroke::new(1.0, Color32::from_rgb(51, 52, 59)))
                                    .corner_radius(10.0)
                                    .inner_margin(8.0)
                                    .show(ui, |ui| {
                                        for (command, description) in matches {
                                            if ui
                                                .selectable_label(
                                                    false,
                                                    format!("{command:<12}  {description}"),
                                                )
                                                .clicked()
                                            {
                                                self.composer = command.into();
                                                self.request_focus = true;
                                            }
                                        }
                                    });
                                ui.add_space(5.0);
                            }
                        }

                        let frame = egui::Frame::default()
                            .fill(Color32::from_rgb(28, 29, 33))
                            .stroke(Stroke::new(1.0, Color32::from_rgb(63, 64, 72)))
                            .corner_radius(16.0)
                            .inner_margin(Margin::symmetric(13, 10));
                        let composer_frame = frame.show(ui, |ui| {
                            let width = ui.available_width().max(160.0);
                            let rows = composer_rows(&self.composer, width);
                            let composer_id = egui::Id::new("main_composer");
                            let response = ui.add_sized(
                                [width, rows as f32 * 22.0 + 8.0],
                                TextEdit::multiline(&mut self.composer)
                                    .id(composer_id)
                                    .frame(egui::Frame::NONE)
                                    .desired_rows(rows)
                                    .desired_width(f32::INFINITY)
                                    .hint_text("Ask GnomeAI…")
                                    .return_key(KeyboardShortcut::new(
                                        Modifiers::SHIFT,
                                        Key::Enter,
                                    )),
                            );
                            if response.clicked_by(egui::PointerButton::Middle) {
                                if let Some(text) = primary_selection_text() {
                                    replace_text_selection(
                                        &ctx,
                                        composer_id,
                                        &mut self.composer,
                                        &text,
                                    );
                                }
                            }
                            response.context_menu(|ui| {
                                if ui.button("Cut").clicked() {
                                    response.request_focus();
                                    ctx.send_viewport_cmd(egui::ViewportCommand::RequestCut);
                                    ui.close();
                                }
                                if ui.button("Copy").clicked() {
                                    response.request_focus();
                                    ctx.send_viewport_cmd(egui::ViewportCommand::RequestCopy);
                                    ui.close();
                                }
                                if ui.button("Paste").clicked() {
                                    response.request_focus();
                                    ctx.send_viewport_cmd(egui::ViewportCommand::RequestPaste);
                                    ui.close();
                                }
                                #[cfg(all(
                                    unix,
                                    not(any(target_os = "macos", target_os = "android"))
                                ))]
                                if ui.button("Paste primary selection").clicked() {
                                    if let Some(text) = primary_selection_text() {
                                        replace_text_selection(
                                            &ctx,
                                            composer_id,
                                            &mut self.composer,
                                            &text,
                                        );
                                    }
                                    ui.close();
                                }
                                if ui.button("Select all").clicked() {
                                    select_all_text(&ctx, composer_id, &self.composer);
                                    response.request_focus();
                                    ui.close();
                                }
                            });
                            if self.request_focus {
                                response.request_focus();
                                self.request_focus = false;
                            }
                            let enter = response.has_focus()
                                && ui.input_mut(|input| {
                                    input.consume_key(Modifiers::NONE, Key::Enter)
                                });
                            let ctrl_enter = response.has_focus()
                                && ui.input_mut(|input| {
                                    input.consume_key(Modifiers::CTRL, Key::Enter)
                                });

                            ui.add_space(3.0);
                            ui.horizontal(|ui| {
                                if composer_icon_button(ui, ComposerIcon::Attach, false)
                                    .on_hover_text("Attach a file")
                                    .clicked()
                                {
                                    self.choose_attachment();
                                }
                                let web = if self.web_search_enabled {
                                    "web on"
                                } else {
                                    "web off"
                                };
                                ui.label(
                                    RichText::new(format!("{} · {web}", self.sandbox))
                                        .small()
                                        .color(Color32::from_rgb(126, 127, 136)),
                                );
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    if self.busy {
                                        if composer_icon_button(ui, ComposerIcon::Stop, true)
                                            .on_hover_text("Stop")
                                            .clicked()
                                        {
                                            self.send(Op::Interrupt);
                                        }
                                    } else if composer_icon_button(
                                        ui,
                                        ComposerIcon::Send,
                                        !self.composer.trim().is_empty()
                                            || self.pending_attachment.is_some(),
                                    )
                                    .on_hover_text("Send (Enter)")
                                    .clicked()
                                    {
                                        self.dispatch_composer();
                                    }
                                    ui.label(
                                        RichText::new(&self.model)
                                            .small()
                                            .color(Color32::from_rgb(126, 127, 136)),
                                    );
                                });
                            });

                            if enter || ctrl_enter {
                                self.dispatch_composer();
                            }
                        });
                        let clicked_inside = ctx.input(|input| {
                            input.pointer.any_click()
                                && input.pointer.interact_pos().is_some_and(|position| {
                                    composer_frame.response.rect.contains(position)
                                })
                        });
                        if clicked_inside {
                            ctx.memory_mut(|memory| {
                                memory.request_focus(egui::Id::new("main_composer"));
                            });
                        }

                        ui.add_space(2.0);
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new("Enter sends · Shift+Enter adds a new line")
                                    .small()
                                    .color(Color32::from_rgb(83, 84, 92)),
                            );
                            if let Some(status) = &self.login_status {
                                ui.label(
                                    RichText::new(status)
                                        .small()
                                        .color(Color32::from_rgb(128, 190, 245)),
                                );
                            }
                        });
                    });
                });
            });
    }

    fn dialogs(&mut self, ctx: &egui::Context) {
        self.approval_dialog(ctx);
        self.privilege_dialog(ctx);
        self.device_login_dialog(ctx);
        self.provider_dialog(ctx);
        self.model_dialog(ctx);
        self.session_dialog(ctx);
        self.whatsapp_conversations_dialog(ctx);
        self.whatsapp_dialog(ctx);
        self.nodes_dialog(ctx);
        self.settings_dialog(ctx);
        self.help_dialog(ctx);
    }

    fn device_login_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.device_login.as_mut() else {
            return;
        };
        let mut open = true;
        let mut copy_code = false;
        let mut open_browser = false;
        egui::Window::new("Connect OpenAI Codex")
            .id(egui::Id::new("codex_device_login_v1"))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size(Vec2::new(420.0, 210.0))
            .min_size(Vec2::new(340.0, 180.0))
            .show(ctx, |ui| {
                ui.label("The sign-in page was opened in your browser.");
                ui.label("Enter this one-time code:");
                ui.add_space(4.0);
                ui.label(
                    RichText::new(&dialog.user_code)
                        .monospace()
                        .size(24.0)
                        .strong(),
                );
                ui.add_space(4.0);
                ui.label(
                    RichText::new(&dialog.verification_url)
                        .small()
                        .color(Color32::from_rgb(128, 190, 245)),
                );
                if let Some(error) = &dialog.browser_error {
                    ui.colored_label(
                        Color32::LIGHT_RED,
                        format!("The browser could not be opened automatically: {error}"),
                    );
                }
                ui.horizontal(|ui| {
                    copy_code = ui.button("Copy code").clicked();
                    open_browser = ui.button("Open browser").clicked();
                });
                ui.label(
                    RichText::new("Waiting for OpenAI confirmation…")
                        .small()
                        .color(Color32::GRAY),
                );
            });

        if copy_code {
            self.copy_request = Some(dialog.user_code.clone());
        }
        if open_browser {
            dialog.browser_error = open_external_url(&dialog.verification_url).err();
        }
        if !open {
            self.device_login = None;
        }
    }

    fn settings_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_settings {
            return;
        }
        let mut open = true;
        let old_sandbox = self.sandbox.clone();
        let old_web_search = self.web_search_enabled;
        let mut save_node_hub = false;
        let mut save_mcp = false;
        let mut remove_mcp = None;

        let dialog_size = Vec2::new(420.0, 360.0);
        egui::Window::new("Settings")
            // A new id intentionally discards geometry saved by the older
            // dialog, whose labelled Sandbox combo could make the body wider
            // than the title bar.
            .id(egui::Id::new("settings_dialog_compact_v3"))
            .open(&mut open)
            .collapsible(false)
            .default_pos(centered_dialog_pos(ctx, dialog_size))
            .default_size(dialog_size)
            .min_size(Vec2::new(340.0, 260.0))
            .max_width(560.0)
            .resizable(true)
            .scroll([false, true])
            .show(ctx, |ui| {
                ui.label(RichText::new("MODEL AND CONNECTIONS").small().strong());
                ui.add_space(5.0);
                egui::Frame::default()
                    .fill(Color32::from_rgb(27, 27, 31))
                    .corner_radius(9.0)
                    .inner_margin(10.0)
                    .show(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(format!("Provider: {}", self.provider));
                            if ui.small_button("Change provider").clicked() {
                                self.show_provider = true;
                            }
                            ui.label(format!("Model: {}", self.model));
                            if ui.small_button("Change model").clicked() {
                                self.show_models = true;
                            }
                            if ui.small_button("WhatsApp").clicked() {
                                self.show_whatsapp = true;
                                self.poll_whatsapp(true);
                            }
                            if ui.small_button("Devices").clicked() {
                                self.show_nodes = true;
                                self.poll_nodes(true);
                            }
                        });
                    });

                ui.add_space(12.0);
                ui.label(RichText::new("MCP SERVERS").small().strong());
                ui.label(
                    RichText::new(
                        "Generic Streamable HTTP or stdio servers. MCP calls are approval-gated; delegated CLIs approve the turn.",
                    )
                    .small()
                    .color(Color32::GRAY),
                );
                for (server_index, server) in self.mcp_servers.iter_mut().enumerate() {
                    ui.add_space(5.0);
                    egui::Frame::default()
                        .fill(Color32::from_rgb(27, 27, 31))
                        .corner_radius(9.0)
                        .inner_margin(10.0)
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.checkbox(&mut server.enabled, "Enabled");
                                ui.add(
                                    TextEdit::singleline(&mut server.name)
                                        .hint_text("server-name")
                                        .desired_width(150.0),
                                );
                                if ui.small_button("Remove").clicked() {
                                    remove_mcp = Some(server_index);
                                }
                            });
                            ui.horizontal(|ui| {
                                ui.label("Transport");
                                egui::ComboBox::from_id_salt(("mcp_transport", server_index))
                                    .selected_text(match server.transport {
                                        McpTransport::StreamableHttp => "Streamable HTTP",
                                        McpTransport::Stdio => "stdio",
                                    })
                                    .show_ui(ui, |ui| {
                                        ui.selectable_value(
                                            &mut server.transport,
                                            McpTransport::StreamableHttp,
                                            "Streamable HTTP",
                                        );
                                        ui.selectable_value(
                                            &mut server.transport,
                                            McpTransport::Stdio,
                                            "stdio",
                                        );
                                    });
                            });
                            match server.transport {
                                McpTransport::StreamableHttp => {
                                    ui.add(
                                        TextEdit::singleline(&mut server.url)
                                            .hint_text("http://127.0.0.1:9239/mcp")
                                            .desired_width(f32::INFINITY),
                                    );
                                    edit_key_value_map(
                                        ui,
                                        &mut server.headers,
                                        "Header",
                                        "+ header",
                                    );
                                }
                                McpTransport::Stdio => {
                                    ui.add(
                                        TextEdit::singleline(&mut server.command)
                                            .hint_text("Executable, e.g. npx")
                                            .desired_width(f32::INFINITY),
                                    );
                                    let mut remove_argument = None;
                                    for (argument_index, argument) in
                                        server.args.iter_mut().enumerate()
                                    {
                                        ui.horizontal(|ui| {
                                            ui.add(
                                                TextEdit::singleline(argument)
                                                    .hint_text("Argument")
                                                    .desired_width(260.0),
                                            );
                                            if ui.small_button("−").clicked() {
                                                remove_argument = Some(argument_index);
                                            }
                                        });
                                    }
                                    if let Some(index) = remove_argument {
                                        server.args.remove(index);
                                    }
                                    if ui.small_button("+ argument").clicked() {
                                        server.args.push(String::new());
                                    }
                                    edit_key_value_map(
                                        ui,
                                        &mut server.env,
                                        "ENV_VAR",
                                        "+ environment variable",
                                    );
                                }
                            }
                            ui.checkbox(
                                &mut server.allow_whatsapp,
                                "Allow this server in WhatsApp (off by default)",
                            );
                        });
                }
                if let Some(index) = remove_mcp {
                    self.mcp_servers.remove(index);
                }
                ui.horizontal_wrapped(|ui| {
                    if ui.small_button("+ BrowserOS").clicked() {
                        self.mcp_servers.push(McpServerConfig {
                            name: "browseros".into(),
                            url: "http://127.0.0.1:9239/mcp".into(),
                            ..McpServerConfig::default()
                        });
                    }
                    if ui.small_button("+ MCP server").clicked() {
                        self.mcp_servers.push(McpServerConfig {
                            name: format!("mcp-server-{}", self.mcp_servers.len() + 1),
                            ..McpServerConfig::default()
                        });
                    }
                    save_mcp = ui.button("Save and reconnect").clicked();
                });

                ui.add_space(12.0);
                ui.label(RichText::new("EXECUTION").small().strong());
                ui.checkbox(&mut self.web_search_enabled, "Web search");
                ui.horizontal(|ui| {
                    ui.label("Sandbox");
                    egui::ComboBox::from_id_salt("settings_sandbox")
                        .selected_text(&self.sandbox)
                        .width(ui.available_width())
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.sandbox, "read-only".into(), "Read only");
                            ui.selectable_value(&mut self.sandbox, "normal".into(), "Normal");
                            ui.selectable_value(
                                &mut self.sandbox,
                                "full-access".into(),
                                "Full access",
                            );
                        });
                });
                ui.add_space(6.0);
                egui::Frame::default()
                    .fill(Color32::from_rgb(27, 27, 31))
                    .corner_radius(9.0)
                    .inner_margin(10.0)
                    .show(ui, |ui| {
                        ui.checkbox(&mut self.node_enabled, "Hub for lightweight devices");
                        ui.horizontal(|ui| {
                            ui.label("Address");
                            ui.add(
                                TextEdit::singleline(&mut self.node_bind)
                                    .hint_text("0.0.0.0")
                                    .desired_width(120.0),
                            );
                            ui.label("Port");
                            ui.add(egui::DragValue::new(&mut self.node_port).range(1..=u16::MAX));
                        });
                        ui.horizontal_wrapped(|ui| {
                            save_node_hub = ui.small_button("Save Hub").clicked();
                            if ui.small_button("Manage devices").clicked() {
                                self.show_nodes = true;
                                self.poll_nodes(true);
                            }
                        });
                        ui.label(
                            RichText::new(
                                "Listener changes take effect after restarting the application.",
                            )
                            .small()
                            .color(Color32::GRAY),
                        );
                    });

                ui.add_space(12.0);
                ui.label(RichText::new("INTERFACE").small().strong());
                ui.checkbox(&mut self.notifications, "Desktop notifications");
                ui.checkbox(&mut self.high_contrast, "High contrast");
                ui.label(
                    RichText::new("Search the current conversation")
                        .small()
                        .color(Color32::GRAY),
                );
                ui.add(
                    TextEdit::singleline(&mut self.search)
                        .id(egui::Id::new("transcript_search"))
                        .hint_text("Search…")
                        .desired_width(f32::INFINITY),
                );

                ui.add_space(12.0);
                ui.label(RichText::new("TOOLS").small().strong());
                ui.horizontal_wrapped(|ui| {
                    if ui.button("Skills").clicked() {
                        self.send(Op::SkillsList);
                    }
                    if ui.button("Install SKILL.md").clicked() {
                        self.choose_skill_file();
                    }
                    if ui.button("Memory").clicked() {
                        self.send(Op::MemoryShow);
                    }
                    if ui.button("Compact context").clicked() {
                        self.send(Op::Compact);
                    }
                    if ui.button("Rollback patches").clicked() {
                        self.send(Op::Rollback);
                    }
                    if ui.button("Diagnostics").clicked() {
                        self.send(Op::Doctor);
                    }
                    if ui.button("Tokens").clicked() {
                        self.show_token_usage();
                    }
                    if ui.button("Export Markdown").clicked() {
                        self.export_conversation();
                    }
                });
                ui.add_space(8.0);
                ui.label(
                    RichText::new(format!(
                        "{} input tokens · {} output tokens",
                        self.tokens_in, self.tokens_out
                    ))
                    .small()
                    .color(Color32::GRAY),
                );
            });

        self.show_settings = open;
        if self.web_search_enabled != old_web_search {
            self.send(Op::SetWebSearch {
                enabled: self.web_search_enabled,
            });
        }
        if self.sandbox != old_sandbox {
            self.send(Op::SetSandbox {
                mode: self.sandbox.clone(),
            });
        }
        if save_node_hub {
            self.send(Op::SetNodeHub {
                enabled: self.node_enabled,
                bind: self.node_bind.trim().to_string(),
                port: self.node_port,
            });
        }
        if save_mcp {
            self.send(Op::SetMcpServers {
                servers: self.mcp_servers.clone(),
            });
        }
    }

    fn approval_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.approval.as_ref() else {
            return;
        };
        let call_id = dialog.call_id.clone();
        let allow_always = dialog.allow_always;
        let mut decision = None;
        egui::Window::new("Approval required")
            .collapsible(false)
            .resizable(true)
            .default_size(Vec2::new(420.0, 190.0))
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(RichText::new(&dialog.command).monospace());
                ui.separator();
                ui.label(&dialog.reason);
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Allow once").clicked() {
                        decision = Some(Decision::Allow);
                    }
                    if allow_always && ui.button("Always allow").clicked() {
                        decision = Some(Decision::AlwaysAllow);
                    }
                    if ui.button("Deny").clicked() {
                        decision = Some(Decision::Deny);
                    }
                });
            });
        if let Some(decision) = decision {
            self.approval = None;
            self.send(Op::Approve { call_id, decision });
        }
    }

    fn privilege_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.privilege.as_mut() else {
            return;
        };
        let mut submit = false;
        let mut cancel = false;
        egui::Window::new("Administrator credential")
            .collapsible(false)
            .resizable(true)
            .default_size(Vec2::new(390.0, 190.0))
            .anchor(egui::Align2::CENTER_CENTER, Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(RichText::new(&dialog.command).monospace());
                ui.label(if dialog.dynamic {
                    format!("Authentication step {}", dialog.attempt)
                } else {
                    format!("Attempt {}", dialog.attempt)
                });
                if let Some(prompt) = &dialog.prompt {
                    ui.label(RichText::new(prompt).strong());
                }
                if let Some(message) = &dialog.message {
                    ui.colored_label(Color32::LIGHT_RED, message);
                }
                let response = ui.add(
                    TextEdit::singleline(&mut dialog.credential)
                        .password(true)
                        .hint_text(if dialog.dynamic {
                            "Credential or challenge response"
                        } else {
                            "Password"
                        }),
                );
                if response.lost_focus() && ui.input(|input| input.key_pressed(Key::Enter)) {
                    submit = true;
                }
                if dialog.keyring_available {
                    ui.checkbox(&mut dialog.remember, "Remember in system keyring");
                }
                ui.horizontal(|ui| {
                    submit |= ui.button("Continue").clicked();
                    cancel |= ui.button("Cancel").clicked();
                });
            });
        if submit || cancel {
            let dialog = self.privilege.take().expect("dialog still exists");
            self.send(Op::ProvidePrivilegeCredential {
                request_id: dialog.request_id,
                credential: (!cancel && !dialog.credential.is_empty())
                    .then(|| SecretString::new(dialog.credential)),
                remember: !cancel && dialog.remember && dialog.keyring_available,
            });
        }
    }

    fn provider_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_provider {
            return;
        }
        let mut open = true;
        let mut apply = false;
        let mut login = None;
        let dialog_size = Vec2::new(410.0, 280.0);
        egui::Window::new("Provider")
            // Discard geometry saved by the older labelled combo, which
            // could make the body wider than the title bar.
            .id(egui::Id::new("provider_dialog_compact_v3"))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_pos(centered_dialog_pos(ctx, dialog_size))
            .default_size(dialog_size)
            .min_size(Vec2::new(340.0, 220.0))
            .max_width(540.0)
            .scroll([false, true])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Provider");
                    egui::ComboBox::from_id_salt("provider_selector")
                        .selected_text(PROVIDERS[self.provider_index].name)
                        .width(ui.available_width())
                        .show_ui(ui, |ui| {
                            for (index, provider) in PROVIDERS.iter().enumerate() {
                                ui.selectable_value(
                                    &mut self.provider_index,
                                    index,
                                    format!("{} — {}", provider.name, provider.description),
                                );
                            }
                        });
                });
                let provider = PROVIDERS[self.provider_index];
                ui.label(RichText::new(provider.description).color(Color32::GRAY));
                if provider.id == "custom" {
                    ui.label("OpenAI-compatible base URL");
                    ui.text_edit_singleline(&mut self.provider_base_url);
                }
                match provider.auth {
                    AuthKind::Account => {
                        ui.label(
                            "The session is stored by the official application and reused while it remains valid.",
                        );
                        if ui.button("Activate account").clicked() {
                            login = Some(provider.id.to_string());
                        }
                    }
                    AuthKind::ApiKey | AuthKind::OptionalApiKey => {
                        ui.label(if provider.auth == AuthKind::ApiKey {
                            "API key"
                        } else {
                            "API key (optional)"
                        });
                        ui.add(
                            TextEdit::singleline(&mut self.provider_api_key)
                                .password(true)
                                .hint_text("Leave blank to reuse the saved key")
                                .desired_width(f32::INFINITY),
                        );
                        ui.label(
                            RichText::new(
                                "A new key is stored privately; leaving the field blank reuses the existing key.",
                            )
                            .small()
                            .color(Color32::GRAY),
                        );
                        apply = ui.button("Use provider").clicked();
                    }
                }
            });
        self.show_provider = open;
        if apply {
            let provider = PROVIDERS[self.provider_index];
            let key = self.provider_api_key.trim().to_string();
            // An empty field deliberately asks the core to reuse this
            // provider's owner-only saved credential. If none exists, the
            // core returns the normal "API key required" error.
            self.send(Op::SetProvider {
                provider_id: provider.id.into(),
                api_key: (!key.is_empty()).then(|| SecretString::new(key)),
                base_url: (provider.id == "custom")
                    .then(|| self.provider_base_url.trim().to_string()),
            });
        }
        if let Some(provider_id) = login {
            self.spawn_login(provider_id);
        }
    }

    fn model_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_models {
            return;
        }
        let mut open = true;
        let mut selected = None;
        let dialog_size = Vec2::new(400.0, 300.0);
        egui::Window::new("Model")
            .id(egui::Id::new("model_dialog_compact_v2"))
            .open(&mut open)
            .default_pos(centered_dialog_pos(ctx, dialog_size))
            .default_size(dialog_size)
            .min_size(Vec2::new(320.0, 220.0))
            .max_width(560.0)
            .resizable(true)
            .show(ctx, |ui| {
                ui.add(
                    TextEdit::singleline(&mut self.model_filter)
                        .hint_text("Filter models…")
                        .desired_width(f32::INFINITY),
                );
                ui.separator();
                let filter = self.model_filter.trim().to_lowercase();
                ScrollArea::vertical().show(ui, |ui| {
                    if self.models.is_empty() {
                        ui.label("No provider model list is available. Use /model MODEL.");
                    }
                    for model in &self.models {
                        if !filter.is_empty() && !model.to_lowercase().contains(&filter) {
                            continue;
                        }
                        if ui.selectable_label(model == &self.model, model).clicked() {
                            selected = Some(model.clone());
                        }
                    }
                });
            });
        self.show_models = open;
        if let Some(model) = selected {
            self.model = model.clone();
            self.show_models = false;
            self.send(Op::SetModel { model });
        }
    }

    fn session_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_sessions {
            return;
        }
        let mut open = true;
        let mut op = None;
        egui::Window::new("Sessions")
            .open(&mut open)
            .default_size(Vec2::new(520.0, 340.0))
            .resizable(true)
            .show(ctx, |ui| {
                if self.sessions.is_empty() {
                    ui.label("No saved sessions.");
                }
                ScrollArea::vertical().show(ui, |ui| {
                    for session in &self.sessions {
                        egui::Frame::default()
                            .fill(if session.is_current {
                                Color32::from_rgb(35, 48, 58)
                            } else {
                                Color32::from_rgb(27, 27, 31)
                            })
                            .corner_radius(8.0)
                            .inner_margin(8.0)
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.vertical(|ui| {
                                        ui.label(
                                            RichText::new(
                                                session.title.as_deref().unwrap_or(&session.id),
                                            )
                                            .strong(),
                                        );
                                        ui.label(
                                            RichText::new(format!(
                                                "{} · {} · {} turns",
                                                session.workspace.display(),
                                                session.model,
                                                session.turns
                                            ))
                                            .small()
                                            .color(Color32::GRAY),
                                        );
                                    });
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        if ui.button("Delete").clicked() {
                                            op = Some(Op::DeleteSession {
                                                id: session.id.clone(),
                                            });
                                        }
                                        if ui.button("Rename").clicked() {
                                            self.rename_session = Some((
                                                session.id.clone(),
                                                session.title.clone().unwrap_or_default(),
                                            ));
                                        }
                                        if ui.button("Resume").clicked() {
                                            op = Some(Op::ResumeSession {
                                                id: session.id.clone(),
                                            });
                                        }
                                    });
                                });
                            });
                        ui.add_space(6.0);
                    }
                });
            });
        self.show_sessions = open;
        if let Some(op) = op {
            self.send(op);
        }
        if let Some((id, title)) = self.rename_session.as_mut() {
            let mut save = false;
            let mut cancel = false;
            egui::Window::new("Rename session")
                .collapsible(false)
                .resizable(true)
                .default_size(Vec2::new(360.0, 135.0))
                .show(ctx, |ui| {
                    ui.text_edit_singleline(title);
                    ui.horizontal(|ui| {
                        save = ui.button("Save").clicked();
                        cancel = ui.button("Cancel").clicked();
                    });
                });
            if save {
                let id = id.clone();
                let title = title.trim().to_string();
                self.rename_session = None;
                self.send(Op::RenameSession { id, title });
            } else if cancel {
                self.rename_session = None;
            }
        }
    }

    fn whatsapp_conversations_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_whatsapp_conversations {
            return;
        }
        let mut open = true;
        let mut open_settings = false;
        let mut refresh = false;
        let mut selected_chat = None;
        let conversations = self.whatsapp_conversations.clone();
        let selected = self.whatsapp_selected_chat.clone();
        let active_chat = self.whatsapp_active_chat.clone();
        let assistant_name = self.whatsapp_assistant_name.clone();
        let connected = whatsapp_bool(&self.whatsapp_status, "connected");
        let pending = self.whatsapp_conversations_pending;
        let feedback = self.whatsapp_feedback.clone();

        egui::Window::new("WhatsApp conversations")
            .id(egui::Id::new("whatsapp_conversations_v1"))
            .open(&mut open)
            .default_size(Vec2::new(860.0, 590.0))
            .min_size(Vec2::new(620.0, 400.0))
            .resizable(true)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    status_badge(ui, "Connected", connected);
                    if pending {
                        ui.spinner();
                        ui.label(RichText::new("Updating…").small().color(Color32::GRAY));
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        open_settings = ui.button("Connection settings").clicked();
                        refresh = ui.button("Refresh").clicked();
                    });
                });
                if let Some(message) = feedback.as_deref().filter(|message| !message.is_empty()) {
                    ui.label(RichText::new(message).small().color(Color32::LIGHT_BLUE));
                }
                ui.separator();

                let content_height = ui.available_height().max(300.0);
                ui.horizontal_top(|ui| {
                    ui.allocate_ui(Vec2::new(220.0, content_height), |ui| {
                        ui.label(
                            RichText::new("CHATS")
                                .size(10.0)
                                .strong()
                                .color(Color32::from_rgb(120, 120, 128)),
                        );
                        ui.add_space(4.0);
                        ScrollArea::vertical()
                            .id_salt("whatsapp_chat_list")
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                if conversations.is_empty() {
                                    ui.label(
                                        RichText::new(if pending {
                                            "Loading conversations…"
                                        } else {
                                            "No WhatsApp conversations yet. Incoming messages will appear here."
                                        })
                                        .small()
                                        .color(Color32::GRAY),
                                    );
                                }
                                for (id, title) in &conversations {
                                    let display_title = title
                                        .strip_prefix("WhatsApp - ")
                                        .unwrap_or(title)
                                        .trim();
                                    if ui
                                        .selectable_label(
                                            selected.as_deref() == Some(id.as_str()),
                                            ellipsize(display_title, 28),
                                        )
                                        .on_hover_text(title)
                                        .clicked()
                                    {
                                        selected_chat = Some(id.clone());
                                    }
                                }
                            });
                    });
                    ui.separator();
                    ui.allocate_ui(Vec2::new(ui.available_width(), content_height), |ui| {
                        let raw_title = active_chat
                            .as_ref()
                            .and_then(|chat| chat.get("title"))
                            .and_then(Value::as_str)
                            .unwrap_or("WhatsApp conversation");
                        let title = raw_title
                            .strip_prefix("WhatsApp - ")
                            .unwrap_or(raw_title);
                        ui.heading(title);
                        ui.add_space(4.0);
                        ScrollArea::vertical()
                            .id_salt(("whatsapp_transcript", selected.as_deref().unwrap_or("none")))
                            .auto_shrink([false, false])
                            .stick_to_bottom(true)
                            .wheel_scroll_multiplier(Vec2::ZERO)
                            .show(ui, |ui| {
                                forward_mouse_wheel_to_parent_scroll(ui);
                                let Some(messages) = active_chat
                                    .as_ref()
                                    .and_then(|chat| chat.get("messages"))
                                    .and_then(Value::as_array)
                                else {
                                    ui.label(
                                        RichText::new(if pending {
                                            "Loading messages…"
                                        } else {
                                            "Choose a WhatsApp conversation."
                                        })
                                        .color(Color32::GRAY),
                                    );
                                    return;
                                };
                                let chat_id = selected.as_deref().unwrap_or("whatsapp");
                                let mut visible = 0usize;
                                for (index, message) in messages.iter().enumerate() {
                                    if render_whatsapp_message(
                                        ui,
                                        chat_id,
                                        index,
                                        message,
                                        &assistant_name,
                                    ) {
                                        visible += 1;
                                        ui.add_space(8.0);
                                    }
                                }
                                if visible == 0 {
                                    ui.label(
                                        RichText::new("This WhatsApp conversation is empty.")
                                            .color(Color32::GRAY),
                                    );
                                }
                            });
                    });
                });
            });
        self.show_whatsapp_conversations = open;

        if open_settings {
            self.show_whatsapp = true;
            self.poll_whatsapp(true);
        }
        if refresh {
            self.poll_whatsapp_conversations(true);
        }
        if let Some(id) = selected_chat {
            if self.whatsapp_selected_chat.as_deref() != Some(id.as_str()) {
                self.whatsapp_selected_chat = Some(id);
                self.whatsapp_active_chat = None;
                self.poll_whatsapp_conversations(true);
            }
        }
    }

    fn whatsapp_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_whatsapp {
            return;
        }
        let mut open = true;
        let mut save = false;
        let mut stop = false;
        let mut reload = false;
        let mut refresh_qr = false;
        let mut send_test = false;
        let mut open_conversations = false;
        let connected = whatsapp_bool(&self.whatsapp_status, "connected");
        let running = whatsapp_bool(&self.whatsapp_status, "bridge_running");
        let authenticated = whatsapp_bool(&self.whatsapp_status, "authenticated");
        let qr = whatsapp_text(&self.whatsapp_status, "qr").to_string();
        let own_phone = whatsapp_text(&self.whatsapp_status, "own_phone").to_string();
        let own_jid = whatsapp_text(&self.whatsapp_status, "own_jid").to_string();
        let bridge_error = whatsapp_text(&self.whatsapp_status, "last_error").to_string();
        if connected && self.whatsapp_test_jid.is_empty() && !own_jid.is_empty() {
            self.whatsapp_test_jid = own_jid.clone();
        }

        egui::Window::new("WhatsApp")
            .open(&mut open)
            .default_size(Vec2::new(480.0, 460.0))
            .resizable(true)
            .scroll([false, true])
            .show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    status_badge(ui, "Service", running);
                    status_badge(ui, "Authenticated", authenticated);
                    status_badge(ui, "Connected", connected);
                    if !own_phone.is_empty() {
                        ui.label(RichText::new(format!("Number: +{own_phone}")).small());
                    }
                    open_conversations = ui.button("View conversations").clicked();
                });
                if let Some(error) = &self.whatsapp_service.launch_error {
                    ui.colored_label(Color32::LIGHT_RED, error);
                }
                if !bridge_error.is_empty() {
                    ui.colored_label(Color32::LIGHT_RED, &bridge_error);
                }
                if let Some(message) = &self.whatsapp_feedback {
                    ui.label(RichText::new(message).color(Color32::LIGHT_BLUE));
                }

                ui.separator();
                ui.checkbox(&mut self.whatsapp_enabled, "Enable WhatsApp integration");
                ui.label("Assistant display name");
                ui.add(
                    TextEdit::singleline(&mut self.whatsapp_assistant_name)
                        .desired_width(f32::INFINITY),
                );
                ui.checkbox(
                    &mut self.whatsapp_has_own_number,
                    "The assistant uses a dedicated WhatsApp number",
                );
                ui.label("Allowed conversations (JIDs, one per line)");
                ui.add(
                    TextEdit::multiline(&mut self.whatsapp_allowed_jids)
                        .desired_rows(3)
                        .desired_width(f32::INFINITY)
                        .hint_text("40700000000@s.whatsapp.net"),
                );
                ui.label(
                    RichText::new(
                        "If the list is empty, only messages to the account itself are accepted.",
                    )
                    .small()
                    .color(Color32::GRAY),
                );
                ui.horizontal(|ui| {
                    save = ui.button("Save and apply").clicked();
                    stop = ui.button("Stop").clicked();
                    reload = ui.button("Restart bridge").clicked();
                    if self.whatsapp_enabled && !connected {
                        refresh_qr = ui.button("New QR code").clicked();
                    }
                });

                if !qr.is_empty() && !connected {
                    ui.separator();
                    ui.heading("Scan the QR code in WhatsApp");
                    render_qr(ui, &qr);
                }

                if connected {
                    ui.separator();
                    ui.collapsing("Send a test message", |ui| {
                        ui.label("Recipient JID");
                        ui.add(
                            TextEdit::singleline(&mut self.whatsapp_test_jid)
                                .desired_width(f32::INFINITY)
                                .hint_text(if own_jid.is_empty() {
                                    "40700000000@s.whatsapp.net".to_string()
                                } else {
                                    own_jid.clone()
                                }),
                        );
                        ui.label("Message");
                        ui.add(
                            TextEdit::multiline(&mut self.whatsapp_test_message)
                                .desired_rows(2)
                                .desired_width(f32::INFINITY),
                        );
                        send_test = ui.button("Send on WhatsApp").clicked();
                    });
                }
                ui.separator();
                ui.collapsing("Recent log", |ui| {
                    ui.label(
                        RichText::new(self.whatsapp_log_file.display().to_string())
                            .small()
                            .color(Color32::GRAY),
                    );
                    let log = read_log_tail(&self.whatsapp_log_file, 12 * 1024);
                    if log.is_empty() {
                        ui.label("The log is empty.");
                    } else {
                        ScrollArea::vertical().max_height(180.0).show(ui, |ui| {
                            ui.add(
                                egui::Label::new(RichText::new(log).monospace()).selectable(true),
                            );
                        });
                    }
                });
            });
        self.show_whatsapp = open;

        if save {
            let mut allowed_jids = self
                .whatsapp_allowed_jids
                .split(|character| character == '\n' || character == ',')
                .map(str::trim)
                .filter(|jid| !jid.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>();
            allowed_jids.sort();
            allowed_jids.dedup();
            self.whatsapp_feedback = Some("Saving settings…".into());
            self.send(Op::SetWhatsApp {
                enabled: self.whatsapp_enabled,
                assistant_name: self.whatsapp_assistant_name.clone(),
                has_own_number: self.whatsapp_has_own_number,
                allowed_jids,
            });
        }
        if stop {
            self.whatsapp_enabled = false;
            self.send(Op::SetWhatsApp {
                enabled: false,
                assistant_name: self.whatsapp_assistant_name.clone(),
                has_own_number: self.whatsapp_has_own_number,
                allowed_jids: self
                    .whatsapp_allowed_jids
                    .lines()
                    .map(str::trim)
                    .filter(|jid| !jid.is_empty())
                    .map(str::to_string)
                    .collect(),
            });
        }
        if reload {
            self.reload_whatsapp_service();
        }
        if refresh_qr {
            self.refresh_whatsapp_qr();
        }
        if send_test {
            self.send_whatsapp_test();
        }
        if open_conversations {
            self.show_whatsapp_conversations = true;
            self.poll_whatsapp_conversations(true);
        }
    }

    fn nodes_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_nodes {
            return;
        }
        let mut open = true;
        let mut copy_command = None;
        let mut policy_change = None;
        let nodes = self
            .node_status
            .get("nodes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let enroll = format!(
            "gnomeai-node enroll --server http://PC-IP:{} --token {} --name NAME",
            self.node_port, self.node_enrollment_token
        );
        let enroll_root = format!("{enroll} --allow-root");

        egui::Window::new("Devices")
            .id(egui::Id::new("nodes_dialog_v1"))
            .open(&mut open)
            .default_size(Vec2::new(540.0, 430.0))
            .min_size(Vec2::new(400.0, 280.0))
            .resizable(true)
            .scroll([false, true])
            .show(ctx, |ui| {
                if !self.node_enabled {
                    ui.colored_label(
                        Color32::LIGHT_YELLOW,
                        "The Hub is disabled. Enable it in Settings and restart the application.",
                    );
                } else {
                    ui.horizontal_wrapped(|ui| {
                        status_badge(ui, "Hub", self.node_feedback.is_none());
                        ui.label(format!("{}:{}", self.node_bind, self.node_port));
                        if ui.small_button("Refresh").clicked() {
                            self.poll_nodes(true);
                        }
                    });
                }
                if let Some(message) = &self.node_feedback {
                    ui.label(RichText::new(message).color(Color32::LIGHT_BLUE));
                }

                ui.separator();
                ui.label(RichText::new("CONNECT A CLIENT").small().strong());
                ui.label(
                    RichText::new(
                        "Install the minimal package on the device, then run one of the commands below. Replace PC-IP and NAME.",
                    )
                    .small()
                    .color(Color32::GRAY),
                );
                ui.horizontal_wrapped(|ui| {
                    if ui.button("Copy normal command").clicked() {
                        copy_command = Some(enroll.clone());
                    }
                    if ui.button("Copy with local root").clicked() {
                        copy_command = Some(enroll_root.clone());
                    }
                });
                ui.label(
                    RichText::new(
                        "The client runs in the foreground: manually or with runit, OpenRC, s6, or another supervisor. It does not depend on systemd.",
                    )
                    .small()
                    .color(Color32::GRAY),
                );
                ui.collapsing("runit example", |ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new("#!/bin/sh\nexec /usr/bin/gnomeai-node run")
                                .monospace(),
                        )
                        .selectable(true),
                    );
                });
                ui.label(
                    RichText::new(
                        "Use a trusted network/VPN (for example Tailscale); do not expose the HTTP port directly to the internet.",
                    )
                    .small()
                    .color(Color32::LIGHT_YELLOW),
                );

                ui.separator();
                ui.label(RichText::new("PAIRED DEVICES").small().strong());
                if nodes.is_empty() {
                    ui.label("No devices paired yet.");
                }
                for node in &nodes {
                    let node_id = node
                        .get("node_id")
                        .and_then(Value::as_str)
                        .unwrap_or("?")
                        .to_string();
                    let name = node
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or(&node_id);
                    let online = node
                        .get("online")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let os = node.get("os").and_then(Value::as_str).unwrap_or("?");
                    let arch = node.get("arch").and_then(Value::as_str).unwrap_or("?");
                    let init = node
                        .get("init_system")
                        .and_then(Value::as_str)
                        .unwrap_or("manual");
                    let root_available = node
                        .get("root_available")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let original_policy = node
                        .get("root_policy")
                        .and_then(Value::as_str)
                        .unwrap_or("ask")
                        .to_string();
                    let mut policy = original_policy.clone();
                    let policy_label = match policy.as_str() {
                        "disabled" => "Blocked",
                        "session" => "Session",
                        "always" => "Always allowed",
                        _ => "Ask",
                    }
                    .to_string();
                    egui::Frame::default()
                        .fill(Color32::from_rgb(27, 27, 31))
                        .corner_radius(9.0)
                        .inner_margin(10.0)
                        .show(ui, |ui| {
                            ui.horizontal_wrapped(|ui| {
                                status_badge(ui, name, online);
                                ui.label(
                                    RichText::new(format!("{os} · {arch} · {init}"))
                                        .small()
                                        .color(Color32::GRAY),
                                );
                            });
                            ui.label(
                                RichText::new(format!("ID: {node_id}"))
                                    .monospace()
                                    .small(),
                            );
                            ui.horizontal_wrapped(|ui| {
                                ui.label(if root_available {
                                    "Local root available"
                                } else {
                                    "Local root unavailable"
                                });
                                ui.label("Root policy");
                                egui::ComboBox::from_id_salt(format!(
                                    "node_root_policy_{node_id}"
                                ))
                                .selected_text(policy_label)
                                .show_ui(ui, |ui| {
                                    ui.selectable_value(
                                        &mut policy,
                                        "disabled".into(),
                                        "Blocked",
                                    );
                                    ui.selectable_value(
                                        &mut policy,
                                        "ask".into(),
                                        "Ask for every command",
                                    );
                                    ui.selectable_value(
                                        &mut policy,
                                        "session".into(),
                                        "Allowed for the Hub session",
                                    );
                                    ui.selectable_value(
                                        &mut policy,
                                        "always".into(),
                                        "Always allowed",
                                    );
                                });
                            });
                        });
                    if policy != original_policy {
                        policy_change = Some((node_id, policy));
                    }
                    ui.add_space(7.0);
                }
            });
        self.show_nodes = open;
        if let Some(command) = copy_command {
            self.copy_request = Some(command);
            self.node_feedback = Some("The command was copied.".into());
        }
        if let Some((node_id, policy)) = policy_change {
            self.set_node_policy(node_id, policy);
        }
    }

    fn help_dialog(&mut self, ctx: &egui::Context) {
        if !self.show_help {
            return;
        }
        egui::Window::new("Commands")
            .open(&mut self.show_help)
            .default_size(Vec2::new(440.0, 340.0))
            .resizable(true)
            .scroll([false, true])
            .show(ctx, |ui| {
                for (command, description) in COMMANDS {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(*command).monospace().strong());
                        ui.label(*description);
                    });
                }
                ui.separator();
                ui.label(
                    "Enter sends · Shift+Enter inserts a newline · Ctrl+. stops · Esc closes dialogs",
                );
            });
    }
}

impl eframe::App for GuiApp {
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = root.ctx().clone();
        self.drain_events();
        self.handle_dropped_files(&ctx);
        apply_contrast(&ctx, self.high_contrast);
        if let Some(text) = self.copy_request.take() {
            ctx.copy_text(text);
        }

        let composer_id = egui::Id::new("main_composer");
        if ctx.memory(|memory| memory.has_focus(composer_id)) {
            if self.composer.is_empty()
                && ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::ArrowUp))
                && !self.history.is_empty()
            {
                let index = self.history.len() - 1;
                self.history_pos = Some(index);
                self.composer = self.history[index].clone();
            } else if self.history_pos.is_some()
                && ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::ArrowDown))
            {
                let next = self.history_pos.unwrap_or(0) + 1;
                if next >= self.history.len() {
                    self.history_pos = None;
                    self.composer.clear();
                } else {
                    self.history_pos = Some(next);
                    self.composer = self.history[next].clone();
                }
            }
            let prefix = self.composer.trim();
            if prefix.starts_with('/')
                && !prefix.contains(char::is_whitespace)
                && ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Tab))
            {
                if let Some((command, _)) = COMMANDS
                    .iter()
                    .find(|(command, _)| command.starts_with(prefix))
                {
                    self.composer = (*command).into();
                }
            }
        }
        if self.show_whatsapp || self.show_whatsapp_conversations || self.whatsapp_enabled {
            self.poll_whatsapp(false);
        }
        if self.show_whatsapp_conversations {
            self.poll_whatsapp_conversations(false);
        }
        if self.show_nodes {
            self.poll_nodes(false);
        }
        self.sidebar(root);
        self.activity_panel(root);
        self.status_bar(root);
        self.composer(root);
        self.transcript(root);
        self.dialogs(&ctx);

        if ctx.input(|input| input.key_pressed(Key::Escape)) {
            self.show_help = false;
            self.show_models = false;
            self.show_provider = false;
            self.show_sessions = false;
            self.show_whatsapp = false;
            self.show_whatsapp_conversations = false;
            self.show_nodes = false;
            self.show_settings = false;
        }
        if ctx.input_mut(|input| input.consume_key(Modifiers::CTRL, Key::F)) {
            self.show_settings = true;
            ctx.memory_mut(|memory| memory.request_focus(egui::Id::new("transcript_search")));
        }
        if ctx.input_mut(|input| input.consume_key(Modifiers::CTRL, Key::Period)) && self.busy {
            self.send(Op::Interrupt);
        }
        if ctx.input_mut(|input| input.consume_key(Modifiers::CTRL, Key::L)) {
            ctx.memory_mut(|memory| memory.request_focus(composer_id));
        }
        if self.quit_requested {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        ctx.request_repaint_after(Duration::from_millis(if self.busy { 40 } else { 120 }));
    }
}

fn configure_style(ctx: &egui::Context) {
    let mut style = (*ctx.global_style()).clone();
    style.visuals = app_visuals(false);
    style.spacing.item_spacing = Vec2::new(8.0, 7.0);
    style.spacing.button_padding = Vec2::new(11.0, 6.0);
    style.spacing.interact_size = Vec2::new(40.0, 32.0);
    style.spacing.window_margin = Margin::same(16);
    style.spacing.menu_margin = Margin::same(10);
    style.spacing.extra_text_line_spacing = 1.0;
    style.animation_time = 0.12;
    style.text_styles.insert(
        egui::TextStyle::Heading,
        FontId::new(21.0, FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Body,
        FontId::new(14.5, FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Monospace,
        FontId::new(12.8, FontFamily::Monospace),
    );
    style.text_styles.insert(
        egui::TextStyle::Button,
        FontId::new(14.0, FontFamily::Proportional),
    );
    style.text_styles.insert(
        egui::TextStyle::Small,
        FontId::new(11.5, FontFamily::Proportional),
    );
    ctx.set_global_style(style);
}

fn apply_contrast(ctx: &egui::Context, high_contrast: bool) {
    ctx.set_visuals(app_visuals(high_contrast));
}

fn app_visuals(high_contrast: bool) -> egui::Visuals {
    let mut visuals = egui::Visuals::dark();
    let border = Color32::from_rgb(54, 55, 62);
    let surface = Color32::from_rgb(31, 32, 36);
    let surface_hover = Color32::from_rgb(42, 43, 49);
    let text = Color32::from_rgb(232, 233, 237);
    let muted = Color32::from_rgb(154, 155, 164);
    let accent = Color32::from_rgb(105, 205, 153);

    visuals.override_text_color = Some(text);
    visuals.weak_text_color = Some(muted);
    visuals.panel_fill = Color32::from_rgb(17, 18, 20);
    visuals.window_fill = Color32::from_rgb(25, 26, 29);
    visuals.window_stroke = Stroke::new(1.0, border);
    visuals.window_corner_radius = egui::CornerRadius::same(12);
    visuals.menu_corner_radius = egui::CornerRadius::same(10);
    visuals.faint_bg_color = Color32::from_rgb(28, 29, 33);
    visuals.extreme_bg_color = Color32::from_rgb(13, 14, 16);
    visuals.text_edit_bg_color = Some(Color32::from_rgb(20, 21, 24));
    visuals.code_bg_color = Color32::from_rgb(24, 25, 29);
    visuals.hyperlink_color = Color32::from_rgb(128, 190, 245);
    visuals.warn_fg_color = Color32::from_rgb(232, 185, 92);
    visuals.error_fg_color = Color32::from_rgb(241, 112, 127);
    visuals.selection.bg_fill = Color32::from_rgba_unmultiplied(78, 149, 112, 150);
    visuals.selection.stroke = Stroke::new(1.0, Color32::from_rgb(188, 238, 211));
    visuals.interact_cursor = Some(egui::CursorIcon::PointingHand);
    visuals.indent_has_left_vline = true;

    visuals.widgets.noninteractive.bg_fill = Color32::from_rgb(25, 26, 29);
    visuals.widgets.noninteractive.weak_bg_fill = Color32::TRANSPARENT;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, border);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, text);
    visuals.widgets.noninteractive.corner_radius = egui::CornerRadius::same(8);

    visuals.widgets.inactive.bg_fill = surface;
    visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(28, 29, 33);
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, border);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, Color32::from_rgb(207, 208, 214));
    visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(8);

    visuals.widgets.hovered.bg_fill = surface_hover;
    visuals.widgets.hovered.weak_bg_fill = surface_hover;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, Color32::from_rgb(74, 76, 85));
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.5, Color32::WHITE);
    visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(8);

    visuals.widgets.active.bg_fill = Color32::from_rgb(48, 49, 55);
    visuals.widgets.active.weak_bg_fill = Color32::from_rgb(48, 49, 55);
    visuals.widgets.active.bg_stroke = Stroke::new(1.0, accent);
    visuals.widgets.active.fg_stroke = Stroke::new(1.5, Color32::WHITE);
    visuals.widgets.active.corner_radius = egui::CornerRadius::same(8);
    visuals.widgets.open = visuals.widgets.active;

    if high_contrast {
        visuals.override_text_color = Some(Color32::WHITE);
        visuals.panel_fill = Color32::BLACK;
        visuals.window_fill = Color32::from_rgb(5, 5, 5);
        visuals.extreme_bg_color = Color32::BLACK;
        visuals.widgets.noninteractive.fg_stroke.color = Color32::WHITE;
        visuals.widgets.inactive.fg_stroke.color = Color32::WHITE;
    }
    visuals
}

#[derive(Debug, Clone, Copy)]
enum NavIcon {
    More,
    Folder,
    Model,
    WhatsApp,
    Settings,
}

fn new_conversation_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 40.0), Sense::click());
    ui.painter().rect_filled(
        rect,
        10.0,
        if response.hovered() {
            Color32::WHITE
        } else {
            Color32::from_rgb(235, 236, 238)
        },
    );
    let icon_center = rect.left_center() + Vec2::new(16.0, 0.0);
    let ink = Color32::from_rgb(28, 29, 32);
    let stroke = Stroke::new(1.7, ink);
    ui.painter().line_segment(
        [
            icon_center + Vec2::new(-4.5, 0.0),
            icon_center + Vec2::new(4.5, 0.0),
        ],
        stroke,
    );
    ui.painter().line_segment(
        [
            icon_center + Vec2::new(0.0, -4.5),
            icon_center + Vec2::new(0.0, 4.5),
        ],
        stroke,
    );
    ui.painter().text(
        rect.left_center() + Vec2::new(35.0, 0.0),
        egui::Align2::LEFT_CENTER,
        text,
        FontId::new(14.0, FontFamily::Proportional),
        ink,
    );
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn suggestion_button(ui: &mut egui::Ui, title: &str, subtitle: &str) -> egui::Response {
    let width = ui.available_width().min(520.0);
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 58.0), Sense::click());
    ui.painter().rect_filled(
        rect,
        12.0,
        if response.hovered() {
            Color32::from_rgb(36, 37, 42)
        } else {
            Color32::from_rgb(27, 28, 32)
        },
    );
    let left = rect.left() + 15.0;
    ui.painter().text(
        egui::pos2(left, rect.top() + 20.0),
        egui::Align2::LEFT_CENTER,
        title,
        FontId::new(13.8, FontFamily::Proportional),
        Color32::from_rgb(230, 231, 235),
    );
    ui.painter().text(
        egui::pos2(left, rect.top() + 40.0),
        egui::Align2::LEFT_CENTER,
        subtitle,
        FontId::new(11.2, FontFamily::Proportional),
        Color32::from_rgb(134, 135, 144),
    );
    let arrow = rect.right_center() + Vec2::new(-17.0, 0.0);
    let arrow_color = if response.hovered() {
        Color32::from_rgb(105, 205, 153)
    } else {
        Color32::from_rgb(126, 127, 136)
    };
    let stroke = Stroke::new(1.4, arrow_color);
    ui.painter().line_segment(
        [arrow + Vec2::new(-4.0, 0.0), arrow + Vec2::new(4.0, 0.0)],
        stroke,
    );
    ui.painter().line_segment(
        [arrow + Vec2::new(1.0, -3.0), arrow + Vec2::new(4.0, 0.0)],
        stroke,
    );
    ui.painter().line_segment(
        [arrow + Vec2::new(1.0, 3.0), arrow + Vec2::new(4.0, 0.0)],
        stroke,
    );
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

#[derive(Debug, Clone, Copy)]
enum ComposerIcon {
    Attach,
    Send,
    Stop,
}

fn composer_icon_button(ui: &mut egui::Ui, icon: ComposerIcon, accented: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(30.0), Sense::click());
    let fill = match icon {
        ComposerIcon::Stop => {
            if response.hovered() {
                Color32::from_rgb(177, 65, 78)
            } else {
                Color32::from_rgb(142, 50, 62)
            }
        }
        _ if accented => {
            if response.hovered() {
                Color32::from_rgb(127, 222, 173)
            } else {
                Color32::from_rgb(105, 205, 153)
            }
        }
        _ => {
            if response.hovered() {
                Color32::from_rgb(55, 56, 63)
            } else {
                Color32::from_rgb(39, 40, 45)
            }
        }
    };
    ui.painter().circle_filled(rect.center(), 15.0, fill);
    let ink = if accented {
        Color32::from_rgb(17, 36, 26)
    } else {
        Color32::from_rgb(210, 211, 217)
    };
    let stroke = Stroke::new(1.7, ink);
    match icon {
        ComposerIcon::Attach => {
            ui.painter().line_segment(
                [
                    rect.center() + Vec2::new(-4.5, 0.0),
                    rect.center() + Vec2::new(4.5, 0.0),
                ],
                stroke,
            );
            ui.painter().line_segment(
                [
                    rect.center() + Vec2::new(0.0, -4.5),
                    rect.center() + Vec2::new(0.0, 4.5),
                ],
                stroke,
            );
        }
        ComposerIcon::Send => {
            ui.painter().line_segment(
                [
                    rect.center() + Vec2::new(0.0, 5.0),
                    rect.center() + Vec2::new(0.0, -5.0),
                ],
                stroke,
            );
            ui.painter().line_segment(
                [
                    rect.center() + Vec2::new(-4.0, -1.0),
                    rect.center() + Vec2::new(0.0, -5.0),
                ],
                stroke,
            );
            ui.painter().line_segment(
                [
                    rect.center() + Vec2::new(4.0, -1.0),
                    rect.center() + Vec2::new(0.0, -5.0),
                ],
                stroke,
            );
        }
        ComposerIcon::Stop => {
            ui.painter().rect_filled(
                egui::Rect::from_center_size(rect.center(), Vec2::splat(7.0)),
                1.5,
                Color32::WHITE,
            );
        }
    }
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn nav_button(ui: &mut egui::Ui, text: &str, selected: bool, icon: NavIcon) -> egui::Response {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 38.0), Sense::click());
    let hovered = response.hovered();
    if selected || hovered {
        ui.painter().rect_filled(
            rect,
            9.0,
            if selected {
                Color32::from_rgb(41, 42, 47)
            } else {
                Color32::from_rgb(31, 32, 36)
            },
        );
    }
    let foreground = if selected || hovered {
        Color32::WHITE
    } else {
        Color32::from_rgb(198, 199, 207)
    };
    draw_nav_icon(
        ui.painter(),
        rect.left_center() + Vec2::new(15.0, 0.0),
        icon,
        if selected {
            Color32::from_rgb(105, 205, 153)
        } else {
            foreground
        },
    );
    ui.painter().text(
        rect.left_center() + Vec2::new(34.0, 0.0),
        egui::Align2::LEFT_CENTER,
        ellipsize(text, 26),
        FontId::new(14.0, FontFamily::Proportional),
        foreground,
    );
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn draw_nav_icon(painter: &egui::Painter, center: egui::Pos2, icon: NavIcon, color: Color32) {
    let stroke = Stroke::new(1.5, color);
    match icon {
        NavIcon::More => {
            for offset in [-5.0, 0.0, 5.0] {
                painter.circle_filled(center + Vec2::new(offset, 0.0), 1.4, color);
            }
        }
        NavIcon::Folder => {
            let points = [
                center + Vec2::new(-6.0, -4.0),
                center + Vec2::new(-1.5, -4.0),
                center + Vec2::new(0.5, -2.0),
                center + Vec2::new(6.0, -2.0),
                center + Vec2::new(6.0, 5.0),
                center + Vec2::new(-6.0, 5.0),
                center + Vec2::new(-6.0, -4.0),
            ];
            painter.add(egui::Shape::line(points.to_vec(), stroke));
        }
        NavIcon::Model => {
            painter.circle_stroke(center, 6.0, stroke);
            painter.circle_filled(center, 2.0, color);
        }
        NavIcon::WhatsApp => {
            painter.circle_stroke(center + Vec2::new(0.0, -0.5), 6.0, stroke);
            painter.line_segment(
                [center + Vec2::new(-4.0, 4.0), center + Vec2::new(-5.5, 7.0)],
                stroke,
            );
            painter.circle_filled(center + Vec2::new(0.0, -0.5), 1.4, color);
        }
        NavIcon::Settings => {
            for (y, knob) in [(-4.0, -2.0), (0.0, 3.0), (4.0, -1.0)] {
                painter.line_segment(
                    [center + Vec2::new(-6.0, y), center + Vec2::new(6.0, y)],
                    stroke,
                );
                painter.circle_filled(center + Vec2::new(knob, y), 1.9, color);
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionRowAction {
    None,
    Resume,
    AskDelete,
    ConfirmDelete,
    CancelDelete,
}

fn session_row(
    ui: &mut egui::Ui,
    id: &str,
    title: &str,
    project: &str,
    selected: bool,
    confirming_delete: bool,
) -> SessionRowAction {
    let height = if confirming_delete { 62.0 } else { 52.0 };
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::click());
    let row_id = ui.make_persistent_id(("session_row", id));
    let painter = ui.painter();

    if confirming_delete {
        painter.rect_filled(rect, 9.0, Color32::from_rgb(54, 29, 33));
        painter.text(
            egui::pos2(rect.left() + 11.0, rect.top() + 17.0),
            egui::Align2::LEFT_CENTER,
            ellipsize(title, 27),
            FontId::new(13.0, FontFamily::Proportional),
            Color32::from_rgb(242, 220, 223),
        );
        painter.text(
            egui::pos2(rect.left() + 11.0, rect.bottom() - 16.0),
            egui::Align2::LEFT_CENTER,
            "Delete?",
            FontId::new(11.5, FontFamily::Proportional),
            Color32::from_rgb(222, 150, 159),
        );

        let delete_rect = egui::Rect::from_min_size(
            egui::pos2(rect.right() - 65.0, rect.bottom() - 29.0),
            Vec2::new(57.0, 23.0),
        );
        let cancel_rect = egui::Rect::from_min_size(
            egui::pos2(delete_rect.left() - 65.0, delete_rect.top()),
            Vec2::new(59.0, 23.0),
        );
        let delete = ui.interact(delete_rect, row_id.with("confirm"), Sense::click());
        let cancel = ui.interact(cancel_rect, row_id.with("cancel"), Sense::click());
        painter.rect_filled(
            delete_rect,
            6.0,
            if delete.hovered() {
                Color32::from_rgb(176, 63, 77)
            } else {
                Color32::from_rgb(139, 48, 60)
            },
        );
        painter.rect_filled(
            cancel_rect,
            6.0,
            if cancel.hovered() {
                Color32::from_rgb(69, 52, 56)
            } else {
                Color32::from_rgb(61, 43, 47)
            },
        );
        painter.text(
            delete_rect.center(),
            egui::Align2::CENTER_CENTER,
            "Delete",
            FontId::new(11.0, FontFamily::Proportional),
            Color32::WHITE,
        );
        painter.text(
            cancel_rect.center(),
            egui::Align2::CENTER_CENTER,
            "Cancel",
            FontId::new(11.0, FontFamily::Proportional),
            Color32::from_rgb(222, 211, 214),
        );
        return if delete.clicked() {
            SessionRowAction::ConfirmDelete
        } else if cancel.clicked() {
            SessionRowAction::CancelDelete
        } else {
            SessionRowAction::None
        };
    }

    if selected || response.hovered() {
        painter.rect_filled(
            rect,
            9.0,
            if selected {
                Color32::from_rgb(39, 40, 45)
            } else {
                Color32::from_rgb(31, 32, 36)
            },
        );
    }
    if selected {
        painter.rect_filled(
            egui::Rect::from_min_size(
                egui::pos2(rect.left(), rect.top() + 10.0),
                Vec2::new(3.0, rect.height() - 20.0),
            ),
            2.0,
            Color32::from_rgb(105, 205, 153),
        );
    }

    let left = rect.left() + 11.0;
    painter.text(
        egui::pos2(left, rect.top() + 18.0),
        egui::Align2::LEFT_CENTER,
        ellipsize(title, 25),
        FontId::new(13.5, FontFamily::Proportional),
        Color32::from_rgb(225, 225, 230),
    );
    painter.text(
        egui::pos2(left, rect.top() + 38.0),
        egui::Align2::LEFT_CENTER,
        ellipsize(project, 29),
        FontId::new(11.0, FontFamily::Proportional),
        Color32::from_rgb(125, 126, 135),
    );

    let delete_rect = egui::Rect::from_center_size(
        egui::pos2(rect.right() - 17.0, rect.center().y),
        Vec2::splat(26.0),
    );
    let delete = ui.interact(delete_rect, row_id.with("delete"), Sense::click());
    if response.hovered() || delete.hovered() {
        if delete.hovered() {
            painter.rect_filled(delete_rect, 6.0, Color32::from_rgb(61, 43, 47));
        }
        draw_trash_icon(
            painter,
            delete_rect.center(),
            if delete.hovered() {
                Color32::from_rgb(242, 125, 138)
            } else {
                Color32::from_rgb(154, 154, 164)
            },
        );
    }

    if delete.clicked() {
        SessionRowAction::AskDelete
    } else if response.clicked() {
        SessionRowAction::Resume
    } else {
        SessionRowAction::None
    }
}

fn draw_trash_icon(painter: &egui::Painter, center: egui::Pos2, color: Color32) {
    let stroke = Stroke::new(1.4, color);
    painter.line_segment(
        [
            center + Vec2::new(-5.0, -4.0),
            center + Vec2::new(5.0, -4.0),
        ],
        stroke,
    );
    painter.line_segment(
        [
            center + Vec2::new(-3.5, -6.0),
            center + Vec2::new(3.5, -6.0),
        ],
        stroke,
    );
    painter.line_segment(
        [
            center + Vec2::new(-4.0, -2.0),
            center + Vec2::new(-3.0, 6.0),
        ],
        stroke,
    );
    painter.line_segment(
        [center + Vec2::new(4.0, -2.0), center + Vec2::new(3.0, 6.0)],
        stroke,
    );
    painter.line_segment(
        [center + Vec2::new(-3.0, 6.0), center + Vec2::new(3.0, 6.0)],
        stroke,
    );
}

fn ellipsize(text: &str, max_characters: usize) -> String {
    if text.chars().count() <= max_characters {
        return text.to_string();
    }
    let mut shortened = text
        .chars()
        .take(max_characters.saturating_sub(1))
        .collect::<String>();
    shortened.push('…');
    shortened
}

fn diff_file_names(diff: &str) -> Vec<String> {
    let mut files = Vec::new();
    for line in diff.lines() {
        let file = line
            .strip_prefix("+++ b/")
            .or_else(|| line.strip_prefix("--- a/"));
        let Some(file) = file else {
            continue;
        };
        if file == "/dev/null" || files.iter().any(|known| known == file) {
            continue;
        }
        files.push(file.to_string());
    }
    files
}

fn diff_line_color(line: &str) -> Color32 {
    if line.starts_with('+') && !line.starts_with("+++") {
        Color32::from_rgb(100, 210, 140)
    } else if line.starts_with('-') && !line.starts_with("---") {
        Color32::from_rgb(235, 105, 105)
    } else if line.starts_with("@@") {
        Color32::LIGHT_BLUE
    } else {
        Color32::from_rgb(165, 165, 173)
    }
}

fn status_indicator(ui: &mut egui::Ui, label: &str, color: Color32) {
    let width = (label.chars().count() as f32 * 6.5 + 22.0).clamp(74.0, 150.0);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 27.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, 13.5, Color32::from_rgb(27, 28, 32));
    ui.painter()
        .circle_filled(rect.left_center() + Vec2::new(10.0, 0.0), 3.2, color);
    ui.painter().text(
        rect.left_center() + Vec2::new(18.0, 0.0),
        egui::Align2::LEFT_CENTER,
        label,
        FontId::new(11.5, FontFamily::Proportional),
        Color32::from_rgb(202, 203, 210),
    );
}

fn status_badge(ui: &mut egui::Ui, label: &str, active: bool) {
    let color = if active {
        Color32::from_rgb(80, 200, 130)
    } else {
        Color32::from_rgb(135, 135, 145)
    };
    ui.label(
        RichText::new(format!("{} {label}", if active { "●" } else { "○" }))
            .small()
            .color(color),
    );
}

fn whatsapp_bool(status: &Value, key: &str) -> bool {
    status.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn centered_dialog_pos(ctx: &egui::Context, size: Vec2) -> egui::Pos2 {
    let viewport = ctx.content_rect();
    egui::pos2(
        (viewport.center().x - size.x * 0.5).max(viewport.left() + 12.0),
        (viewport.center().y - size.y * 0.5).max(viewport.top() + 12.0),
    )
}

fn whatsapp_text<'a>(status: &'a Value, key: &str) -> &'a str {
    status.get(key).and_then(Value::as_str).unwrap_or("")
}

fn render_qr(ui: &mut egui::Ui, payload: &str) {
    let Ok(code) = QrCode::new(payload.as_bytes()) else {
        ui.colored_label(Color32::LIGHT_RED, "The QR code cannot be displayed.");
        return;
    };
    let modules = code.width();
    let quiet_zone = 4usize;
    let total = modules + quiet_zone * 2;
    let side = 240.0;
    let module_side = side / total as f32;
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(side), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 4.0, Color32::WHITE);
    for y in 0..modules {
        for x in 0..modules {
            if code[(x, y)] != QrColor::Dark {
                continue;
            }
            let min = rect.min
                + Vec2::new(
                    (x + quiet_zone) as f32 * module_side,
                    (y + quiet_zone) as f32 * module_side,
                );
            painter.rect_filled(
                egui::Rect::from_min_size(min, Vec2::splat(module_side + 0.15)),
                0.0,
                Color32::BLACK,
            );
        }
    }
}

fn companion_executable(name: &str) -> std::result::Result<PathBuf, String> {
    let current = std::env::current_exe()
        .map_err(|error| format!("Cannot determine the current executable: {error}"))?;
    let candidate = current.with_file_name(name);
    if candidate.is_file() {
        Ok(candidate)
    } else {
        Err(format!(
            "WhatsApp service is missing: {}. Run `cargo build --bins` or reinstall the package.",
            candidate.display()
        ))
    }
}

async fn http_json(request: reqwest::RequestBuilder) -> std::result::Result<Value, String> {
    let response = request
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|error| format!("WhatsApp service is not responding: {error}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|error| format!("Invalid WhatsApp response: {error}"))?;
    let value = serde_json::from_str::<Value>(&text)
        .unwrap_or_else(|_| serde_json::json!({"message": text}));
    if status.is_success() {
        Ok(value)
    } else {
        Err(value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("The WhatsApp operation failed.")
            .to_string())
    }
}

fn whatsapp_chat_summaries(value: &Value) -> Vec<(String, String)> {
    let mut chats = value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|chat| {
            let id = chat.get("id")?.as_str()?.trim();
            if !id.starts_with("wa_") {
                return None;
            }
            let title = chat
                .get("title")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|title| !title.is_empty())
                .unwrap_or(id);
            Some((id.to_string(), title.to_string()))
        })
        .collect::<Vec<_>>();
    chats.sort_by(|left, right| {
        left.1
            .to_lowercase()
            .cmp(&right.1.to_lowercase())
            .then_with(|| left.0.cmp(&right.0))
    });
    chats
}

fn whatsapp_message_text(message: &Value) -> Option<String> {
    let role = message.get("role").and_then(Value::as_str).unwrap_or("");
    if !matches!(
        role.trim().to_ascii_lowercase().as_str(),
        "user" | "assistant" | "gnome"
    ) {
        return None;
    }
    let content = message.get("content")?;
    let text = match content {
        Value::String(text) => {
            if text.starts_with("[Extracted content from uploaded file:") {
                return None;
            }
            text.trim().to_string()
        }
        Value::Object(object) => {
            let kind = object.get("type").and_then(Value::as_str).unwrap_or("file");
            let filename = object
                .get("filename")
                .and_then(Value::as_str)
                .unwrap_or("attachment");
            format!("[{kind}] {filename}")
        }
        other => other.to_string(),
    };
    (!text.is_empty()).then_some(text)
}

fn render_whatsapp_message(
    ui: &mut egui::Ui,
    chat_id: &str,
    index: usize,
    message: &Value,
    assistant_name: &str,
) -> bool {
    let Some(text) = whatsapp_message_text(message) else {
        return false;
    };
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let incoming = role == "user";
    let author = if incoming {
        message
            .get("sender_name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or("WhatsApp")
    } else if assistant_name.trim().is_empty() {
        "GnomeAI"
    } else {
        assistant_name.trim()
    };
    let timestamp = message
        .get("timestamp")
        .and_then(Value::as_str)
        .map(|value| value.get(..16).unwrap_or(value).replace('T', " "))
        .unwrap_or_default();

    egui::Frame::default()
        .fill(if incoming {
            Color32::from_rgb(28, 48, 39)
        } else {
            Color32::from_rgb(28, 29, 33)
        })
        .stroke(Stroke::new(
            1.0,
            if incoming {
                Color32::from_rgb(48, 78, 64)
            } else {
                Color32::from_rgb(52, 53, 60)
            },
        ))
        .corner_radius(10.0)
        .inner_margin(Margin::symmetric(11, 8))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(author).small().strong().color(if incoming {
                    Color32::from_rgb(115, 214, 159)
                } else {
                    Color32::from_rgb(151, 152, 161)
                }));
                if !timestamp.is_empty() {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(RichText::new(timestamp).small().color(Color32::GRAY));
                    });
                }
            });
            ui.add_space(2.0);
            if incoming {
                selectable_text(
                    ui,
                    ("whatsapp-message", chat_id, index),
                    &text,
                    true,
                    false,
                    None,
                );
            } else {
                let markdown_id = ui.make_persistent_id(("whatsapp-markdown", chat_id, index));
                render_markdown(ui, markdown_id, &text);
            }
        });
    true
}

fn read_log_tail(path: &Path, limit: usize) -> String {
    let Ok(bytes) = std::fs::read(path) else {
        return String::new();
    };
    let start = bytes.len().saturating_sub(limit);
    let mut tail = &bytes[start..];
    if start > 0 {
        if let Some(newline) = tail.iter().position(|byte| *byte == b'\n') {
            tail = &tail[newline + 1..];
        }
    }
    String::from_utf8_lossy(tail).trim().to_string()
}

#[derive(Clone)]
struct RememberedTextSelection {
    range: egui::text::CCursorRange,
    text: String,
}

fn text_in_cursor_range(text: &str, range: egui::text::CCursorRange) -> Option<String> {
    if range.is_empty() {
        return None;
    }
    let [start, end] = range.sorted_cursors();
    let character_count = text.chars().count();
    let start = start.index.0.min(character_count);
    let end = end.index.0.min(character_count);
    let start_byte = text
        .char_indices()
        .nth(start)
        .map(|(offset, _)| offset)
        .unwrap_or(text.len());
    let end_byte = text
        .char_indices()
        .nth(end)
        .map(|(offset, _)| offset)
        .unwrap_or(text.len());
    (start_byte < end_byte).then(|| text[start_byte..end_byte].to_string())
}

fn selectable_text(
    ui: &mut egui::Ui,
    id_salt: impl egui::AsIdSalt,
    complete_text: &str,
    wrap: bool,
    monospace: bool,
    color: Option<Color32>,
) {
    // Label::selectable treats every pointer press as a new selection. A right
    // click therefore collapsed the user's range before the context menu could
    // copy it, and its drag handler also competed with the transcript scroll.
    // An immutable TextEdit has native selection/keyboard handling, never
    // edits the source string, and leaves the mouse wheel to the outer scroll.
    let id = ui.make_persistent_id(id_salt);
    let remembered_id = id.with("remembered-selection");
    let remembered = ui
        .ctx()
        .data(|data| data.get_temp::<RememberedTextSelection>(remembered_id));

    let mut immutable_text = complete_text;
    let mut editor = TextEdit::multiline(&mut immutable_text)
        .id(id)
        .frame(egui::Frame::NONE)
        .margin(Margin::same(0))
        .desired_rows(1)
        .desired_width(if wrap {
            ui.available_width().max(1.0)
        } else {
            f32::INFINITY
        });
    if monospace {
        editor = editor.font(egui::TextStyle::Monospace);
    }
    if let Some(color) = color {
        editor = editor.text_color(color);
    }

    let mut output = editor.show(ui);
    let current = output
        .cursor_range
        .and_then(|range| text_in_cursor_range(complete_text, range).map(|text| (range, text)));

    if let Some((range, text)) = current {
        let selection = RememberedTextSelection { range, text };
        ui.ctx()
            .data_mut(|data| data.insert_temp(remembered_id, selection.clone()));
    } else if output.response.clicked() {
        // A plain left click intentionally clears the old selection. A right
        // click does not, so the context menu can still use the selected range.
        ui.ctx().data_mut(|data| {
            data.remove::<RememberedTextSelection>(remembered_id);
        });
    }

    let selection = ui
        .ctx()
        .data(|data| data.get_temp::<RememberedTextSelection>(remembered_id))
        .or(remembered);

    if output.response.secondary_clicked() {
        output.response.request_focus();
        if let Some(selection) = &selection {
            output.state.cursor.set_char_range(Some(selection.range));
            output.state.clone().store(ui.ctx(), output.response.id);
        }
    }

    output.response.context_menu(|ui| {
        if ui
            .add_enabled(selection.is_some(), egui::Button::new("Copy selection"))
            .clicked()
        {
            if let Some(selection) = &selection {
                ui.ctx().copy_text(selection.text.clone());
            }
            ui.close();
        }
        if ui.button("Copy all").clicked() {
            ui.ctx().copy_text(complete_text.to_string());
            ui.close();
        }
    });
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum MarkdownBlock {
    Heading {
        level: usize,
        text: String,
    },
    Paragraph(String),
    List(Vec<MarkdownListItem>),
    Quote(String),
    Code {
        language: String,
        text: String,
    },
    Table {
        headers: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    Rule,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MarkdownListItem {
    depth: usize,
    marker: String,
    text: String,
}

fn markdown_heading(line: &str) -> Option<(usize, &str)> {
    let trimmed = line.trim_start();
    let hashes = trimmed
        .chars()
        .take_while(|character| *character == '#')
        .count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    trimmed
        .get(hashes..)
        .and_then(|rest| rest.strip_prefix(' '))
        .map(|text| (hashes, text.trim()))
}

fn markdown_list_item(line: &str) -> Option<MarkdownListItem> {
    let indent = line.len().saturating_sub(line.trim_start().len());
    let trimmed = line.trim_start();
    for marker in ["- ", "* ", "+ "] {
        if let Some(text) = trimmed.strip_prefix(marker) {
            let (marker, text) = if let Some(text) = text.strip_prefix("[ ] ") {
                ("☐", text)
            } else if let Some(text) = text
                .strip_prefix("[x] ")
                .or_else(|| text.strip_prefix("[X] "))
            {
                ("☑", text)
            } else {
                ("•", text)
            };
            return Some(MarkdownListItem {
                depth: indent / 2,
                marker: marker.into(),
                text: clean_inline_markdown(text.trim()),
            });
        }
    }

    let digits = trimmed
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .count();
    if digits > 0 {
        let suffix = trimmed.get(digits..)?;
        if let Some(text) = suffix.strip_prefix(". ") {
            return Some(MarkdownListItem {
                depth: indent / 2,
                marker: format!("{}.", &trimmed[..digits]),
                text: clean_inline_markdown(text.trim()),
            });
        }
    }
    None
}

fn markdown_rule(line: &str) -> bool {
    let compact = line
        .trim()
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect::<String>();
    compact.len() >= 3
        && compact
            .chars()
            .next()
            .is_some_and(|first| matches!(first, '-' | '*' | '_'))
        && compact
            .chars()
            .all(|character| Some(character) == compact.chars().next())
}

fn markdown_table_cells(line: &str) -> Vec<String> {
    line.trim()
        .trim_start_matches('|')
        .trim_end_matches('|')
        .split('|')
        .map(|cell| clean_inline_markdown(cell.trim()))
        .collect()
}

fn markdown_table_delimiter(line: &str) -> bool {
    let cells = markdown_table_cells(line);
    cells.len() >= 2
        && cells.iter().all(|cell| {
            let cell = cell.trim().trim_matches(':');
            cell.len() >= 3 && cell.chars().all(|character| character == '-')
        })
}

fn starts_markdown_block(lines: &[&str], index: usize) -> bool {
    let line = lines[index];
    let trimmed = line.trim_start();
    markdown_heading(line).is_some()
        || markdown_list_item(line).is_some()
        || markdown_rule(line)
        || trimmed.starts_with("> ")
        || trimmed == ">"
        || trimmed.starts_with("```")
        || trimmed.starts_with("~~~")
        || (index + 1 < lines.len()
            && line.contains('|')
            && markdown_table_delimiter(lines[index + 1]))
}

fn parse_markdown_blocks(text: &str) -> Vec<MarkdownBlock> {
    let lines = text.lines().collect::<Vec<_>>();
    let mut blocks = Vec::new();
    let mut index = 0;

    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim_start();
        if trimmed.is_empty() {
            index += 1;
            continue;
        }

        if let Some(fence) = ["```", "~~~"]
            .into_iter()
            .find(|fence| trimmed.starts_with(fence))
        {
            let language = trimmed[fence.len()..].trim().to_string();
            index += 1;
            let start = index;
            while index < lines.len() && !lines[index].trim_start().starts_with(fence) {
                index += 1;
            }
            blocks.push(MarkdownBlock::Code {
                language,
                text: lines[start..index].join("\n"),
            });
            if index < lines.len() {
                index += 1;
            }
            continue;
        }

        if let Some((level, heading)) = markdown_heading(line) {
            blocks.push(MarkdownBlock::Heading {
                level,
                text: clean_inline_markdown(heading),
            });
            index += 1;
            continue;
        }

        if index + 1 < lines.len()
            && line.contains('|')
            && markdown_table_delimiter(lines[index + 1])
        {
            let headers = markdown_table_cells(line);
            index += 2;
            let mut rows = Vec::new();
            while index < lines.len() && !lines[index].trim().is_empty() {
                if !lines[index].contains('|') || starts_markdown_block(&lines, index) {
                    break;
                }
                rows.push(markdown_table_cells(lines[index]));
                index += 1;
            }
            blocks.push(MarkdownBlock::Table { headers, rows });
            continue;
        }

        if markdown_list_item(line).is_some() {
            let mut items = Vec::new();
            while index < lines.len() {
                let Some(item) = markdown_list_item(lines[index]) else {
                    break;
                };
                items.push(item);
                index += 1;
            }
            blocks.push(MarkdownBlock::List(items));
            continue;
        }

        if trimmed.starts_with("> ") || trimmed == ">" {
            let mut quote = Vec::new();
            while index < lines.len() {
                let trimmed = lines[index].trim_start();
                let Some(content) = trimmed
                    .strip_prefix("> ")
                    .or_else(|| (trimmed == ">").then_some(""))
                else {
                    break;
                };
                quote.push(clean_inline_markdown(content));
                index += 1;
            }
            blocks.push(MarkdownBlock::Quote(quote.join("\n")));
            continue;
        }

        if markdown_rule(line) {
            blocks.push(MarkdownBlock::Rule);
            index += 1;
            continue;
        }

        let mut paragraph = Vec::new();
        while index < lines.len()
            && !lines[index].trim().is_empty()
            && !starts_markdown_block(&lines, index)
        {
            paragraph.push(lines[index].trim());
            index += 1;
        }
        if paragraph.is_empty() {
            // A malformed Markdown construct must never stall streaming.
            paragraph.push(line.trim());
            index += 1;
        }
        blocks.push(MarkdownBlock::Paragraph(clean_inline_markdown(
            &paragraph.join(" "),
        )));
    }

    blocks
}

fn clean_inline_markdown(text: &str) -> String {
    let mut cleaned = text
        .replace("**", "")
        .replace("__", "")
        .replace("~~", "")
        .replace('`', "");

    // Make Markdown links readable outside a browser without exposing their
    // punctuation. Keep the destination so copying the answer remains useful.
    while let Some(open) = cleaned.find('[') {
        let Some(close_offset) = cleaned[open + 1..].find("](") else {
            break;
        };
        let close = open + 1 + close_offset;
        let url_start = close + 2;
        let Some(end_offset) = cleaned[url_start..].find(')') else {
            break;
        };
        let end = url_start + end_offset;
        let label = &cleaned[open + 1..close];
        let url = &cleaned[url_start..end];
        let replacement = if label == url || url.is_empty() {
            label.to_string()
        } else {
            format!("{label} ({url})")
        };
        cleaned.replace_range(open..=end, &replacement);
    }
    cleaned
}

fn render_markdown(ui: &mut egui::Ui, id: egui::Id, text: &str) {
    for (block_index, block) in parse_markdown_blocks(text).into_iter().enumerate() {
        let block_id = id.with(block_index);
        match block {
            MarkdownBlock::Heading { level, text } => {
                ui.add_space(if level <= 2 { 7.0 } else { 3.0 });
                let size = match level {
                    1 => 23.0,
                    2 => 20.0,
                    3 => 17.0,
                    _ => 15.0,
                };
                ui.add(
                    egui::Label::new(
                        RichText::new(text)
                            .size(size)
                            .strong()
                            .color(Color32::from_rgb(218, 221, 226)),
                    )
                    .selectable(true)
                    .wrap(),
                );
            }
            MarkdownBlock::Paragraph(text) => {
                selectable_text(ui, block_id, &text, true, false, None);
            }
            MarkdownBlock::List(items) => {
                for (item_index, item) in items.into_iter().enumerate() {
                    ui.horizontal_top(|ui| {
                        ui.add_space((item.depth as f32 * 18.0).min(72.0));
                        ui.label(
                            RichText::new(item.marker)
                                .strong()
                                .color(Color32::from_rgb(126, 207, 164)),
                        );
                        ui.vertical(|ui| {
                            selectable_text(
                                ui,
                                block_id.with(item_index),
                                &item.text,
                                true,
                                false,
                                None,
                            );
                        });
                    });
                }
            }
            MarkdownBlock::Quote(text) => {
                ui.horizontal(|ui| {
                    let (bar, _) = ui.allocate_exact_size(Vec2::new(3.0, 22.0), Sense::hover());
                    ui.painter()
                        .rect_filled(bar, 1.5, Color32::from_rgb(105, 205, 153));
                    ui.add_space(6.0);
                    ui.vertical(|ui| {
                        selectable_text(
                            ui,
                            block_id,
                            &text,
                            true,
                            false,
                            Some(Color32::from_rgb(174, 178, 184)),
                        );
                    });
                });
            }
            MarkdownBlock::Code { language, text } => {
                egui::Frame::default()
                    .fill(Color32::from_rgb(12, 13, 15))
                    .stroke(Stroke::new(1.0, Color32::from_rgb(52, 55, 62)))
                    .corner_radius(8.0)
                    .inner_margin(Margin::symmetric(10, 8))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(
                                RichText::new(if language.is_empty() {
                                    "CODE"
                                } else {
                                    language.as_str()
                                })
                                .small()
                                .strong()
                                .color(Color32::from_rgb(126, 207, 164)),
                            );
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui.small_button("Copy code").clicked() {
                                    ui.ctx().copy_text(text.clone());
                                }
                            });
                        });
                        ui.add_space(4.0);
                        selectable_text(ui, block_id, &text, true, true, None);
                    });
            }
            MarkdownBlock::Table { headers, rows } => {
                let columns = headers
                    .len()
                    .max(rows.iter().map(Vec::len).max().unwrap_or(0));
                if columns == 0 {
                    continue;
                }
                egui::Frame::default()
                    .fill(Color32::from_rgb(20, 21, 24))
                    .stroke(Stroke::new(1.0, Color32::from_rgb(52, 55, 62)))
                    .corner_radius(8.0)
                    .inner_margin(Margin::symmetric(8, 7))
                    .show(ui, |ui| {
                        ui.columns(columns, |column_uis| {
                            for (column, column_ui) in column_uis.iter_mut().enumerate() {
                                selectable_text(
                                    column_ui,
                                    block_id.with(("header", column)),
                                    headers.get(column).map(String::as_str).unwrap_or(""),
                                    true,
                                    false,
                                    Some(Color32::from_rgb(126, 207, 164)),
                                );
                            }
                        });
                        ui.separator();
                        for (row_index, row) in rows.iter().enumerate() {
                            if row_index > 0 {
                                ui.add_space(2.0);
                            }
                            ui.columns(columns, |column_uis| {
                                for (column, column_ui) in column_uis.iter_mut().enumerate() {
                                    selectable_text(
                                        column_ui,
                                        block_id.with((row_index, column)),
                                        row.get(column).map(String::as_str).unwrap_or(""),
                                        true,
                                        false,
                                        None,
                                    );
                                }
                            });
                        }
                    });
            }
            MarkdownBlock::Rule => {
                ui.add_space(3.0);
                ui.separator();
                ui.add_space(3.0);
            }
        }
        ui.add_space(5.0);
    }
}

/// Forward the physical mouse wheel to the nearest parent `ScrollArea`.
///
/// egui deliberately ignores its normal wheel path while a child widget owns
/// a pointer drag. Our read-only multiline `TextEdit`s own that drag during
/// text selection, which made the transcript appear frozen whenever the wheel
/// was used over a message. `Ui::scroll_with_delta` is an explicit adjustment,
/// so it remains active both over focused text and while extending a selection.
fn forward_mouse_wheel_to_parent_scroll(ui: &mut egui::Ui) {
    let clip_rect = ui.clip_rect();
    if !ui.rect_contains_pointer(clip_rect) {
        return;
    }

    let wheel = ui.ctx().input(|input| input.smooth_scroll_delta());
    if wheel.y != 0.0 {
        ui.scroll_with_delta(Vec2::new(0.0, wheel.y));
    }
}

fn select_all_text(ctx: &egui::Context, id: egui::Id, text: &str) {
    let mut state = TextEdit::load_state(ctx, id).unwrap_or_default();
    state
        .cursor
        .set_char_range(Some(egui::text::CCursorRange::two(
            egui::text::CCursor::new(0),
            egui::text::CCursor::new(text.chars().count()),
        )));
    TextEdit::store_state(ctx, id, state);
}

fn replace_text_selection(ctx: &egui::Context, id: egui::Id, text: &mut String, inserted: &str) {
    if inserted.is_empty() {
        return;
    }
    let mut state = TextEdit::load_state(ctx, id).unwrap_or_default();
    let end = text.chars().count();
    let range = state
        .cursor
        .char_range()
        .unwrap_or_else(|| egui::text::CCursorRange::one(egui::text::CCursor::new(end)));
    let [start, finish] = range.sorted_cursors();
    let start = start.index.0.min(end);
    let finish = finish.index.0.min(end);
    let start_byte = text
        .char_indices()
        .nth(start)
        .map(|(offset, _)| offset)
        .unwrap_or(text.len());
    let finish_byte = text
        .char_indices()
        .nth(finish)
        .map(|(offset, _)| offset)
        .unwrap_or(text.len());
    text.replace_range(start_byte..finish_byte, inserted);
    let cursor = egui::text::CCursor::new(start + inserted.chars().count());
    state
        .cursor
        .set_char_range(Some(egui::text::CCursorRange::one(cursor)));
    TextEdit::store_state(ctx, id, state);
    ctx.memory_mut(|memory| memory.request_focus(id));
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "android"))))]
fn primary_selection_text() -> Option<String> {
    use arboard::{GetExtLinux, LinuxClipboardKind};

    let mut clipboard = arboard::Clipboard::new().ok()?;
    clipboard
        .get()
        .clipboard(LinuxClipboardKind::Primary)
        .text()
        .ok()
}

#[cfg(not(all(unix, not(any(target_os = "macos", target_os = "android")))))]
fn primary_selection_text() -> Option<String> {
    None
}

fn render_block(ui: &mut egui::Ui, index: usize, block: &Block) {
    match block {
        Block::User(text) => {
            let width = ui.available_width();
            egui::Frame::default()
                .fill(Color32::from_rgb(30, 31, 35))
                .stroke(Stroke::new(1.0, Color32::from_rgb(48, 49, 55)))
                .corner_radius(12.0)
                .inner_margin(Margin::symmetric(14, 11))
                .show(ui, |ui| {
                    ui.set_min_width((width - 30.0).max(120.0));
                    ui.label(
                        RichText::new("YOU")
                            .size(10.0)
                            .strong()
                            .color(Color32::from_rgb(126, 207, 164)),
                    );
                    ui.add_space(2.0);
                    selectable_text(ui, ("user-text", index), text, true, false, None);
                });
        }
        Block::Assistant(text) => {
            let width = ui.available_width();
            ui.horizontal(|ui| {
                let (avatar, _) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::hover());
                ui.painter()
                    .circle_filled(avatar.center(), 14.0, Color32::from_rgb(105, 205, 153));
                ui.painter().text(
                    avatar.center(),
                    egui::Align2::CENTER_CENTER,
                    "G",
                    FontId::new(14.0, FontFamily::Proportional),
                    Color32::from_rgb(18, 31, 24),
                );
                ui.vertical(|ui| {
                    ui.set_min_width((width - 46.0).max(120.0));
                    ui.label(
                        RichText::new("GnomeAI")
                            .size(11.0)
                            .strong()
                            .color(Color32::from_rgb(151, 152, 161)),
                    );
                    ui.add_space(2.0);
                    let markdown_id = ui.make_persistent_id(("assistant-markdown", index));
                    render_markdown(ui, markdown_id, text);
                });
            });
        }
        Block::Reasoning(text) => {
            egui::Frame::default()
                .fill(Color32::from_rgb(23, 24, 28))
                .corner_radius(9.0)
                .inner_margin(Margin::symmetric(10, 7))
                .show(ui, |ui| {
                    egui::CollapsingHeader::new(
                        RichText::new("Reasoning")
                            .italics()
                            .color(Color32::from_rgb(148, 149, 158)),
                    )
                    .id_salt(("reasoning", index))
                    .show(ui, |ui| {
                        selectable_text(
                            ui,
                            ("reasoning-text", index),
                            text,
                            true,
                            false,
                            Some(Color32::from_rgb(153, 154, 163)),
                        );
                    });
                });
        }
        Block::Tool {
            call_id,
            name,
            summary,
            output,
            done,
            ok,
            ms,
        } => {
            let (mark, color) = match (*done, *ok) {
                (false, _) => ("●", Color32::YELLOW),
                (true, true) => ("✓", Color32::from_rgb(80, 200, 130)),
                (true, false) => ("✕", Color32::LIGHT_RED),
            };
            let title = if *done {
                format!("{mark} {name} — {summary} · {ms} ms")
            } else {
                format!("{mark} {name} — {summary}")
            };
            let width = ui.available_width();
            egui::Frame::default()
                .fill(Color32::from_rgb(24, 25, 29))
                .stroke(Stroke::new(
                    1.0,
                    if *done && *ok {
                        Color32::from_rgb(47, 62, 55)
                    } else if *done {
                        Color32::from_rgb(76, 46, 51)
                    } else {
                        Color32::from_rgb(71, 63, 42)
                    },
                ))
                .corner_radius(10.0)
                .inner_margin(Margin::symmetric(11, 8))
                .show(ui, |ui| {
                    ui.set_min_width((width - 24.0).max(120.0));
                    egui::CollapsingHeader::new(RichText::new(title).color(color))
                        .id_salt(("tool", call_id))
                        .default_open(!*done || !*ok)
                        .show(ui, |ui| {
                            if output.is_empty() {
                                ui.label(
                                    RichText::new("Waiting for the result…").color(Color32::GRAY),
                                );
                            } else {
                                egui::Frame::default()
                                    .fill(Color32::from_rgb(15, 16, 18))
                                    .corner_radius(7.0)
                                    .inner_margin(8.0)
                                    .show(ui, |ui| {
                                        // Keep output in the transcript's one vertical scroll
                                        // area. Nested scroll areas stole the wheel while a text
                                        // selection was active.
                                        selectable_text(
                                            ui,
                                            ("tool-output", call_id),
                                            output,
                                            true,
                                            true,
                                            None,
                                        );
                                    });
                            }
                        });
                });
        }
        Block::Diff(diff) => {
            egui::Frame::default()
                .fill(Color32::from_rgb(23, 25, 29))
                .stroke(Stroke::new(1.0, Color32::from_rgb(44, 58, 72)))
                .corner_radius(10.0)
                .inner_margin(Margin::symmetric(11, 8))
                .show(ui, |ui| {
                    egui::CollapsingHeader::new(
                        RichText::new(format!(
                            "Patch changes · {} files",
                            diff_file_names(diff).len()
                        ))
                        .color(Color32::from_rgb(128, 190, 245)),
                    )
                    .id_salt(("diff", index))
                    .show(ui, |ui| {
                        egui::Frame::default()
                            .fill(Color32::from_rgb(14, 15, 17))
                            .corner_radius(7.0)
                            .inner_margin(8.0)
                            .show(ui, |ui| {
                                ScrollArea::horizontal().max_height(320.0).show(ui, |ui| {
                                    for line in diff.lines().take(240) {
                                        ui.label(
                                            RichText::new(line)
                                                .monospace()
                                                .color(diff_line_color(line)),
                                        );
                                    }
                                });
                            });
                    });
                });
        }
        Block::Verify {
            stage,
            passed,
            summary,
        } => {
            let color = if *passed {
                Color32::from_rgb(80, 200, 130)
            } else {
                Color32::LIGHT_RED
            };
            egui::Frame::default()
                .fill(Color32::from_rgb(23, 25, 27))
                .corner_radius(8.0)
                .inner_margin(Margin::symmetric(10, 7))
                .show(ui, |ui| {
                    ui.label(
                        RichText::new(format!(
                            "{} [{stage}] {summary}",
                            if *passed { "✓" } else { "✕" }
                        ))
                        .color(color),
                    );
                });
        }
        Block::Error(error) => {
            egui::Frame::default()
                .fill(Color32::from_rgb(55, 25, 28))
                .stroke(Stroke::new(1.0, Color32::from_rgb(130, 50, 58)))
                .corner_radius(8.0)
                .inner_margin(9.0)
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(RichText::new(error).color(Color32::LIGHT_RED))
                            .selectable(true),
                    );
                });
        }
        Block::Note(note) => {
            ui.horizontal(|ui| {
                let (line, _) = ui.allocate_exact_size(Vec2::new(18.0, 1.0), Sense::hover());
                ui.painter()
                    .rect_filled(line, 0.5, Color32::from_rgb(70, 71, 79));
                ui.add(
                    egui::Label::new(RichText::new(note).color(Color32::from_rgb(137, 138, 147)))
                        .selectable(true)
                        .wrap(),
                );
            });
        }
    }
}

fn composer_rows(text: &str, width: f32) -> usize {
    let characters_per_row = (width / 8.2).floor().max(20.0) as usize;
    text.split('\n')
        .map(|line| (line.chars().count().max(1) + characters_per_row - 1) / characters_per_row)
        .sum::<usize>()
        .clamp(1, 8)
}

fn encode_attachment(path: &Path, prompt: &str) -> std::result::Result<String, String> {
    let metadata = std::fs::metadata(path)
        .map_err(|error| format!("Cannot inspect {}: {error}", path.display()))?;
    if metadata.len() as usize > MAX_ATTACHMENT_BYTES {
        return Err(format!(
            "Attachment is too large (limit {} MiB)",
            MAX_ATTACHMENT_BYTES / (1024 * 1024)
        ));
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("attachment");
    if file_type_from_name(name) == "image" {
        let bytes = std::fs::read(path)
            .map_err(|error| format!("Cannot read {}: {error}", path.display()))?;
        let media_type = mime_guess::from_path(path)
            .first_raw()
            .unwrap_or("application/octet-stream");
        let data = base64::engine::general_purpose::STANDARD.encode(bytes);
        let prompt = if prompt.trim().is_empty() {
            format!("Analyze the attached image “{name}”.")
        } else {
            prompt.trim().to_string()
        };
        return serde_json::to_string(&serde_json::json!([
            {"type": "text", "text": prompt},
            {"type": "image_url", "image_url": {"url": format!("data:{media_type};base64,{data}")}}
        ]))
        .map_err(|error| format!("Cannot prepare the image: {error}"));
    }

    let extracted =
        extract_text_attachment(path).map_err(|error| format!("Cannot read {name}: {error:#}"))?;
    if extracted.trim().is_empty() {
        return Err(format!("No readable text was found in {name}."));
    }
    let prompt = if prompt.trim().is_empty() {
        format!("Analyze the attached file “{name}”.")
    } else {
        prompt.trim().to_string()
    };
    Ok(format!(
        "{prompt}\n\n<attached_file name=\"{name}\">\n{extracted}\n</attached_file>"
    ))
}

fn desktop_notify(title: &str, body: &str) {
    #[cfg(target_os = "linux")]
    let mut command = {
        let mut command = Command::new("notify-send");
        command.args(["--app-name=GnomeAI-RS", "--expire-time=5000", title, body]);
        command
    };
    #[cfg(target_os = "macos")]
    let mut command = {
        let mut command = Command::new("osascript");
        let escaped_title = title.replace('"', "\\\"");
        let escaped_body = body.replace('"', "\\\"");
        command.args([
            "-e",
            &format!("display notification \"{escaped_body}\" with title \"{escaped_title}\""),
        ]);
        command
    };
    let _ = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

fn open_external_url(url: &str) -> std::result::Result<(), String> {
    let url = url.trim();
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err("the authentication address is not HTTP/HTTPS".into());
    }

    #[cfg(target_os = "linux")]
    let attempts: &[(&str, &[&str])] = &[("xdg-open", &[url]), ("gio", &["open", url])];
    #[cfg(target_os = "macos")]
    let attempts: &[(&str, &[&str])] = &[("open", &[url])];

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let mut errors = Vec::new();
        for (program, args) in attempts {
            match Command::new(program)
                .args(*args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(_) => return Ok(()),
                Err(error) => errors.push(format!("{program}: {error}")),
            }
        }
        return Err(errors.join("; "));
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    Err("automatic browser opening is not available on this system".into())
}

fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            value
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(value)
}

fn workspace_path_from_message(text: &str) -> Option<PathBuf> {
    let lower = text.to_lowercase();
    let names_workspace = [
        "workspace",
        "folder",
        "director",
        "proiect",
        "directory",
        "project",
    ]
    .iter()
    .any(|word| lower.contains(word));
    if !names_workspace {
        return None;
    }
    let requests_change = [
        "schimb", "mută", "muta", "seteaz", "change", "switch", "move", "set the",
    ]
    .iter()
    .any(|phrase| lower.contains(phrase));
    let states_location = [
        "proiectul meu este",
        "proiectul meu e",
        "folderul meu este",
        "folderul meu e",
        "directorul meu este",
        "directorul meu e",
        "my project is",
        "my folder is",
        "my directory is",
        "workspace is",
        "workspace-ul este",
        "workspace-ul e",
    ]
    .iter()
    .any(|phrase| lower.contains(phrase));
    let concise_assignment = lower.trim_start().starts_with("workspace:")
        || lower.trim_start().starts_with("folder:")
        || lower.trim_start().starts_with("director:")
        || lower.trim_start().starts_with("project:");
    if !(requests_change || states_location || concise_assignment) {
        return None;
    }
    explicit_path(text).map(PathBuf::from)
}

fn explicit_path(text: &str) -> Option<String> {
    for quote in ['"', '\''] {
        let Some(start) = text.find(quote) else {
            continue;
        };
        let after = &text[start + quote.len_utf8()..];
        let Some(end) = after.find(quote) else {
            continue;
        };
        let candidate = after[..end].trim();
        if looks_like_path(candidate) {
            return Some(candidate.to_string());
        }
    }
    text.split_whitespace().find_map(|part| {
        let candidate = part.trim_matches(|character: char| {
            matches!(
                character,
                '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';' | ':' | '!' | '?'
            )
        });
        let candidate = candidate.strip_suffix('.').unwrap_or(candidate);
        looks_like_path(candidate).then(|| candidate.to_string())
    })
}

fn looks_like_path(value: &str) -> bool {
    value.starts_with('/')
        || value == "~"
        || value.starts_with("~/")
        || value == "."
        || value == ".."
        || value.starts_with("./")
        || value.starts_with("../")
}

fn edit_key_value_map(
    ui: &mut egui::Ui,
    values: &mut BTreeMap<String, String>,
    key_placeholder: &str,
    add_label: &str,
) {
    let entries = std::mem::take(values);
    let mut rebuilt = BTreeMap::new();
    for (index, (mut key, mut value)) in entries.into_iter().enumerate() {
        let mut remove = false;
        ui.horizontal(|ui| {
            ui.add(
                TextEdit::singleline(&mut key)
                    .hint_text(key_placeholder)
                    .desired_width(135.0),
            );
            let sensitive = {
                let normalized = key.to_ascii_lowercase();
                normalized.contains("authorization")
                    || normalized.contains("token")
                    || normalized.contains("secret")
                    || normalized.contains("api-key")
                    || normalized.contains("api_key")
            };
            ui.add(
                TextEdit::singleline(&mut value)
                    .password(sensitive)
                    .hint_text("Value")
                    .desired_width(190.0),
            );
            remove = ui.small_button("−").clicked();
        });
        if !remove && !key.trim().is_empty() {
            let key = key.trim().to_string();
            let unique = if rebuilt.contains_key(&key) {
                format!("{key}_{index}")
            } else {
                key
            };
            rebuilt.insert(unique, value);
        }
    }
    if ui.small_button(add_label).clicked() {
        let mut key = key_placeholder.to_string();
        let mut suffix = 2;
        while rebuilt.contains_key(&key) {
            key = format!("{key_placeholder}_{suffix}");
            suffix += 1;
        }
        rebuilt.insert(key, String::new());
    }
    *values = rebuilt;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composer_grows_for_wrapped_and_hard_lines() {
        assert_eq!(composer_rows("hello", 320.0), 1);
        assert_eq!(composer_rows("hello\nworld", 320.0), 2);
        assert!(composer_rows(&"x".repeat(300), 240.0) > 1);
        assert_eq!(composer_rows(&"x".repeat(10_000), 240.0), 8);
    }

    #[test]
    fn cursor_selection_preserves_partial_unicode_text() {
        let range =
            egui::text::CCursorRange::two(egui::text::CCursor::new(1), egui::text::CCursor::new(5));
        assert_eq!(
            text_in_cursor_range("așezare", range).as_deref(),
            Some("șeza")
        );
        assert!(
            text_in_cursor_range(
                "text",
                egui::text::CCursorRange::one(egui::text::CCursor::new(2))
            )
            .is_none()
        );
    }

    #[test]
    fn markdown_parser_structures_headings_lists_tables_and_code() {
        let blocks = parse_markdown_blocks(
            "# Rezumat\n\nText cu **accent**.\n\n- primul\n- [x] gata\n\n\
             | Model | Viteză |\n| --- | --- |\n| K3 | mare |\n\n\
             ```rust\nfn main() {}\n```",
        );

        assert_eq!(
            blocks,
            vec![
                MarkdownBlock::Heading {
                    level: 1,
                    text: "Rezumat".into(),
                },
                MarkdownBlock::Paragraph("Text cu accent.".into()),
                MarkdownBlock::List(vec![
                    MarkdownListItem {
                        depth: 0,
                        marker: "•".into(),
                        text: "primul".into(),
                    },
                    MarkdownListItem {
                        depth: 0,
                        marker: "☑".into(),
                        text: "gata".into(),
                    },
                ]),
                MarkdownBlock::Table {
                    headers: vec!["Model".into(), "Viteză".into()],
                    rows: vec![vec!["K3".into(), "mare".into()]],
                },
                MarkdownBlock::Code {
                    language: "rust".into(),
                    text: "fn main() {}".into(),
                },
            ]
        );
    }

    #[test]
    fn markdown_parser_keeps_streaming_code_before_the_closing_fence() {
        assert_eq!(
            parse_markdown_blocks("```bash\necho salut"),
            vec![MarkdownBlock::Code {
                language: "bash".into(),
                text: "echo salut".into(),
            }]
        );
    }

    #[test]
    fn markdown_links_remain_copyable_without_raw_punctuation() {
        assert_eq!(
            clean_inline_markdown("Vezi [documentația](https://example.test) și `cod`."),
            "Vezi documentația (https://example.test) și cod."
        );
    }

    #[test]
    fn whatsapp_conversation_list_excludes_regular_web_chats() {
        let value = serde_json::json!([
            {"id": "chat_001", "title": "Browser chat"},
            {"id": "wa_40700_s_whatsapp_net", "title": "WhatsApp - Ana"},
            {"id": "wa_group_g_us", "title": "WhatsApp - Echipa"}
        ]);
        assert_eq!(
            whatsapp_chat_summaries(&value),
            vec![
                ("wa_40700_s_whatsapp_net".into(), "WhatsApp - Ana".into()),
                ("wa_group_g_us".into(), "WhatsApp - Echipa".into()),
            ]
        );
    }

    #[test]
    fn whatsapp_transcript_hides_system_and_extracted_context() {
        assert_eq!(
            whatsapp_message_text(&serde_json::json!({
                "role": "user",
                "content": "Salut"
            }))
            .as_deref(),
            Some("Salut")
        );
        assert!(
            whatsapp_message_text(&serde_json::json!({
                "role": "system",
                "content": "private model context"
            }))
            .is_none()
        );
        assert!(
            whatsapp_message_text(&serde_json::json!({
                "role": "user",
                "content": "[Extracted content from uploaded file: note.txt] secret"
            }))
            .is_none()
        );
    }

    #[test]
    fn native_attachment_reads_text_and_source_files() {
        let path = std::env::temp_dir().join(format!(
            "gnomeai-gui-attachment-{}.rs",
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&path, "fn main() { println!(\"salut\"); }").unwrap();
        let encoded = encode_attachment(&path, "explică acest cod").unwrap();
        let _ = std::fs::remove_file(&path);

        assert!(encoded.contains("explică acest cod"));
        assert!(encoded.contains("<attached_file"));
        assert!(encoded.contains("fn main()"));
    }

    #[test]
    fn unquote_handles_workspace_paths() {
        assert_eq!(unquote("\"/tmp/my project\""), "/tmp/my project");
        assert_eq!(unquote("/tmp/project"), "/tmp/project");
    }

    #[test]
    fn romanian_workspace_intent_is_detected_with_diacritics() {
        assert_eq!(
            workspace_path_from_message("Vreau să schimb folderul în /home/user/Proiectul-Meu"),
            Some(PathBuf::from("/home/user/Proiectul-Meu"))
        );
        assert!(workspace_path_from_message("Citește /tmp/README.md").is_none());
    }

    #[test]
    fn diff_panel_extracts_each_changed_file_once() {
        let diff = "--- a/src/gui.rs\n+++ b/src/gui.rs\n@@ -1 +1 @@\n-old\n+new\n--- a/README.md\n+++ b/README.md";
        assert_eq!(
            diff_file_names(diff),
            vec!["src/gui.rs".to_string(), "README.md".to_string()]
        );
    }

    #[test]
    fn sidebar_labels_preserve_romanian_characters() {
        assert_eq!(ellipsize("New conversation", 40), "New conversation");
        assert_eq!(ellipsize("ăîâșț foarte lung", 8), "ăîâșț f…");
    }

    #[test]
    fn whatsapp_service_avoids_ports_held_by_an_old_instance() {
        let api_guard = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let bridge_guard = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let mut config = AppConfig::default();
        config.port = api_guard.local_addr().unwrap().port();
        config.whatsapp_bridge_port = bridge_guard.local_addr().unwrap().port();

        let (api_port, bridge_port) = native_service_ports(&config);

        assert_ne!(api_port, config.port);
        assert_ne!(bridge_port, config.whatsapp_bridge_port);
        assert_ne!(api_port, bridge_port);
    }
}
