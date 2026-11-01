//! GnomeAI Hub registry and HTTP transport for lightweight remote nodes.
//!
//! Nodes always connect outbound. The transport deliberately has no systemd,
//! D-Bus, desktop-environment or distro dependency.

use anyhow::{Context, Result, bail};
use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Path as AxumPath, Request, State},
    http::{HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Mutex, Notify, oneshot};

use crate::{
    node_protocol::{
        NODE_ONLINE_WINDOW_SECS, NODE_PROTOCOL_VERSION, NodeHello, NodeJob, NodePoll,
        NodePollResponse, NodeRecord, NodeResult, QueueJobRequest, RegisterResponse, RootPolicy,
        SetRootPolicyRequest,
    },
    storage::write_private,
};

const LONG_POLL_SECS: u64 = 25;
const MAX_RESULT_CHARS: usize = 1_000_000;

/// Remembers how each offline node was last reached, so the Devices page can
/// explain a missing node instead of only stating that it is missing.
#[derive(Debug, Default, Clone, serde::Serialize, Deserialize)]
struct SeenPeer {
    /// Source address of the last request this node made to the Hub.
    #[serde(default)]
    address: String,
    /// When the Hub saw that request (`unix_now`).
    #[serde(default)]
    last_unix: u64,
}

#[derive(Clone)]
pub struct NodeHub {
    inner: Arc<Mutex<HubState>>,
    changed: Arc<Notify>,
    store_path: Arc<PathBuf>,
}

#[derive(Default)]
struct HubState {
    nodes: BTreeMap<String, StoredNode>,
    queues: HashMap<String, VecDeque<NodeJob>>,
    waiters: HashMap<String, oneshot::Sender<NodeResult>>,
    /// Per-node record of the last inbound request, including the one that
    /// failed the credential check. Nodes are not trusted enough to have their
    /// failures persisted with the registry.
    seen_peers: HashMap<String, SeenPeer>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredNode {
    hello: NodeHello,
    last_seen_unix: u64,
    #[serde(default)]
    root_policy: RootPolicy,
    /// True when the node registered but has not completed a poll since.
    #[serde(default)]
    pending_probe: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct StoredRegistry {
    version: u32,
    nodes: Vec<StoredNode>,
}

impl NodeHub {
    pub fn open(store_path: PathBuf) -> Result<Self> {
        let mut state = HubState::default();
        if store_path.exists() {
            let stored: StoredRegistry = serde_json::from_slice(&std::fs::read(&store_path)?)
                .context("cannot parse the saved node registry")?;
            for mut node in stored.nodes {
                // A session grant intentionally dies with the Hub process.
                if node.root_policy == RootPolicy::Session {
                    node.root_policy = RootPolicy::Ask;
                }
                state.nodes.insert(node.hello.node_id.clone(), node);
            }
        }
        Ok(Self {
            inner: Arc::new(Mutex::new(state)),
            changed: Arc::new(Notify::new()),
            store_path: Arc::new(store_path),
        })
    }

    async fn persist_locked(&self, state: &HubState) -> Result<()> {
        let registry = StoredRegistry {
            version: NODE_PROTOCOL_VERSION,
            nodes: state.nodes.values().cloned().collect(),
        };
        write_private(&self.store_path, &serde_json::to_vec_pretty(&registry)?)
    }

    pub async fn register(&self, hello: NodeHello) -> Result<RegisterResponse> {
        validate_hello(&hello)?;
        let mut state = self.inner.lock().await;
        let policy = state
            .nodes
            .get(&hello.node_id)
            .map(|node| node.root_policy)
            .unwrap_or_default();
        state.nodes.insert(
            hello.node_id.clone(),
            StoredNode {
                hello,
                last_seen_unix: unix_now(),
                root_policy: policy,
                pending_probe: true,
            },
        );
        self.persist_locked(&state).await?;
        Ok(RegisterResponse {
            ok: true,
            root_policy: policy,
            poll_after_ms: 250,
        })
    }

    pub async fn poll(&self, node_id: &str) -> Result<NodePollResponse> {
        validate_node_id(node_id)?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(LONG_POLL_SECS);
        loop {
            let notified = self.changed.notified();
            {
                let mut state = self.inner.lock().await;
                let policy = {
                    let node = state
                        .nodes
                        .get_mut(node_id)
                        .with_context(|| format!("node `{node_id}` is not enrolled"))?;
                    node.last_seen_unix = unix_now();
                    node.pending_probe = false;
                    node.root_policy
                };
                if let Some(job) = state
                    .queues
                    .entry(node_id.to_string())
                    .or_default()
                    .pop_front()
                {
                    return Ok(NodePollResponse {
                        job: Some(job),
                        root_policy: policy,
                    });
                }
                if tokio::time::Instant::now() >= deadline {
                    return Ok(NodePollResponse {
                        job: None,
                        root_policy: policy,
                    });
                }
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if tokio::time::timeout(remaining, notified).await.is_err() {
                let state = self.inner.lock().await;
                let policy = state
                    .nodes
                    .get(node_id)
                    .map(|node| node.root_policy)
                    .unwrap_or_default();
                return Ok(NodePollResponse {
                    job: None,
                    root_policy: policy,
                });
            }
        }
    }

    pub async fn submit_result(&self, mut result: NodeResult) -> Result<()> {
        validate_node_id(&result.node_id)?;
        result.stdout = cap_text(&result.stdout, MAX_RESULT_CHARS);
        result.stderr = cap_text(&result.stderr, MAX_RESULT_CHARS);
        let sender = {
            let mut state = self.inner.lock().await;
            let node = state
                .nodes
                .get_mut(&result.node_id)
                .with_context(|| format!("node `{}` is not enrolled", result.node_id))?;
            node.last_seen_unix = unix_now();
            state.waiters.remove(&result.job_id)
        };
        if let Some(sender) = sender {
            let _ = sender.send(result);
        }
        Ok(())
    }

    pub async fn list(&self) -> Vec<NodeRecord> {
        let now = unix_now();
        let state = self.inner.lock().await;
        state
            .nodes
            .iter()
            .map(|(node_id, node)| {
                let online = now.saturating_sub(node.last_seen_unix) <= NODE_ONLINE_WINDOW_SECS;
                NodeRecord {
                    hello: node.hello.clone(),
                    last_seen_unix: node.last_seen_unix,
                    online,
                    root_policy: node.root_policy,
                    offline_reason: if online {
                        String::new()
                    } else {
                        offline_reason(node, state.seen_peers.get(node_id), now, state.nodes.len())
                    },
                }
            })
            .collect()
    }

    /// Record where a request for `node_id` came from. Called before the
    /// credential check so a node using a stale or wrong token is still
    /// identified instead of disappearing silently.
    pub async fn note_seen_peer(&self, node_id: &str, address: &str) {
        if validate_node_id(node_id).is_err() {
            return;
        }
        let mut state = self.inner.lock().await;
        if !state.nodes.contains_key(node_id) {
            return;
        }
        let observed = SeenPeer {
            address: address.to_string(),
            last_unix: unix_now(),
        };
        state.seen_peers.insert(node_id.to_string(), observed);
    }

    /// Forget an enrolled node and its queue.
    ///
    /// A node that is still running keeps polling and re-enrolls itself, so the
    /// reply says whether the caller should also stop it on that machine.
    pub async fn remove(&self, node_id: &str) -> Result<bool> {
        validate_node_id(node_id)?;
        let mut state = self.inner.lock().await;
        let node = state
            .nodes
            .remove(node_id)
            .with_context(|| format!("node `{node_id}` is not enrolled"))?;
        let was_online = unix_now().saturating_sub(node.last_seen_unix) <= NODE_ONLINE_WINDOW_SECS;
        let abandoned = state.queues.remove(node_id).unwrap_or_default();
        state.seen_peers.remove(node_id);
        // Drop the waiters of queued jobs: the node that would answer them is
        // gone, so callers should fail now instead of waiting out their timeout.
        for job in abandoned {
            state.waiters.remove(&job.job_id);
        }
        self.persist_locked(&state).await?;
        Ok(was_online)
    }

    /// What this Hub can still do for a node that is not connecting.
    ///
    /// The registry alone cannot explain a missing node: the person also needs
    /// the address a node must be able to reach and the port that has to be
    /// open in the firewall between them.
    pub async fn status(&self) -> Value {
        let state = self.inner.lock().await;
        let now = unix_now();
        let online = state
            .nodes
            .values()
            .filter(|node| now.saturating_sub(node.last_seen_unix) <= NODE_ONLINE_WINDOW_SECS)
            .count();
        json!({
            "ok": true,
            "online_nodes": online,
            "enrolled_nodes": state.nodes.len(),
            "online_window_secs": NODE_ONLINE_WINDOW_SECS,
            "long_poll_secs": LONG_POLL_SECS,
        })
    }

    pub async fn set_root_policy(&self, node_id: &str, policy: RootPolicy) -> Result<()> {
        validate_node_id(node_id)?;
        let mut state = self.inner.lock().await;
        let node = state
            .nodes
            .get_mut(node_id)
            .with_context(|| format!("node `{node_id}` is not enrolled"))?;
        node.root_policy = policy;
        self.persist_locked(&state).await
    }

    pub async fn queue_and_wait(
        &self,
        node_id: &str,
        request: QueueJobRequest,
    ) -> Result<NodeResult> {
        validate_node_id(node_id)?;
        validate_job_request(&request)?;
        let timeout_secs = request.timeout_secs.clamp(1, 3_600);
        let job_id = uuid::Uuid::new_v4().simple().to_string();
        let (sender, receiver) = oneshot::channel();
        {
            let mut state = self.inner.lock().await;
            let (last_seen_unix, root_policy) = state
                .nodes
                .get(node_id)
                .map(|node| (node.last_seen_unix, node.root_policy))
                .with_context(|| format!("node `{node_id}` is not enrolled"))?;
            if unix_now().saturating_sub(last_seen_unix) > NODE_ONLINE_WINDOW_SECS {
                bail!("node `{node_id}` is offline");
            }
            if request.root {
                match root_policy {
                    RootPolicy::Disabled => bail!("root is disabled for node `{node_id}`"),
                    RootPolicy::Ask if !request.root_approved => {
                        bail!("root requires central approval for node `{node_id}`")
                    }
                    RootPolicy::Ask | RootPolicy::Session | RootPolicy::Always => {}
                }
            }
            state.waiters.insert(job_id.clone(), sender);
            state
                .queues
                .entry(node_id.to_string())
                .or_default()
                .push_back(NodeJob {
                    job_id: job_id.clone(),
                    action: request.action,
                    command: request.command,
                    stdin: request.stdin,
                    cwd: request.cwd,
                    timeout_secs,
                    root: request.root,
                });
        }
        self.changed.notify_waiters();
        match tokio::time::timeout(Duration::from_secs(timeout_secs + 15), receiver).await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(_)) => bail!("node result channel closed"),
            Err(_) => {
                self.inner.lock().await.waiters.remove(&job_id);
                bail!("node job timed out after {} seconds", timeout_secs + 15)
            }
        }
    }
}

#[derive(Clone)]
struct NodeServerState {
    hub: NodeHub,
    node_token: Arc<str>,
    admin_token: Arc<str>,
}

pub async fn serve(
    hub: NodeHub,
    bind: SocketAddr,
    node_token: String,
    admin_token: String,
) -> Result<()> {
    if node_token.chars().count() < 32 || admin_token.chars().count() < 32 {
        bail!("node Hub token must contain at least 32 characters")
    }
    let state = NodeServerState {
        hub,
        node_token: Arc::from(node_token),
        admin_token: Arc::from(admin_token),
    };
    let app = Router::new()
        .route("/v1/ping", get(|| async { Json(json!({"ok": true})) }))
        .route("/v1/register", post(register_node))
        .route("/v1/poll", post(poll_node))
        .route("/v1/result", post(submit_result))
        .route("/v1/nodes", get(list_nodes))
        .route("/v1/status", get(hub_status))
        .route("/v1/nodes/{node_id}/jobs", post(queue_job))
        .route("/v1/nodes/{node_id}/policy", post(set_policy))
        .route("/v1/nodes/{node_id}", axum::routing::delete(remove_node))
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            protect_node_api,
        ))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(%bind, "GnomeAI node Hub listening");
    // ConnectInfo carries the peer address into the auth middleware, which is
    // how a rejected node is still attributed to a machine.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

async fn protect_node_api(
    State(state): State<NodeServerState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let (parts, body) = request.into_parts();
    // A rejected request is still evidence: the sender is recorded before the
    // credential check, so "node offline" can become "this address is reaching
    // the Hub with the wrong token". Only node-originated routes carry a node
    // id, and only they are buffered; the body is handed to the real handler
    // unchanged.
    let address = parts
        .extensions
        .get::<axum::extract::ConnectInfo<SocketAddr>>()
        .map(|info| info.0.ip().to_string());
    let body = match peek_node_id(&parts.uri, body, address).await {
        Peek::NotNodeRoute(returned) => returned,
        Peek::Identified(node_id, address, returned) => {
            state.hub.note_seen_peer(&node_id, &address).await;
            returned
        }
        Peek::Oversized => {
            return (
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(json!({"ok": false, "error": "node request body is too large"})),
            )
                .into_response();
        }
    };
    let request = Request::from_parts(parts, body);
    let admin_route = request.uri().path().starts_with("/v1/nodes");
    let (header, expected): (&str, &str) = if admin_route {
        ("x-gnomeai-admin-token", state.admin_token.as_ref())
    } else {
        ("x-gnomeai-node-token", state.node_token.as_ref())
    };
    let supplied = request
        .headers()
        .get(header)
        .and_then(|value: &HeaderValue| value.to_str().ok());
    if !supplied.is_some_and(|value| constant_time_eq(value, expected)) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"ok": false, "error": "invalid node Hub credential"})),
        )
            .into_response();
    }
    next.run(request).await
}

/// Learn a node's address from its own request body.
///
/// The body is buffered and reassembled before the real handler parses it, so
/// the Hub never records a peer from a request the node did not actually send.
async fn record_peer(state: &NodeServerState, request: &Request<Body>) {
    let path = request.uri().path();
    if path != "/v1/poll" && path != "/v1/register" {
        return;
    }
    // Only /v1/poll names its node, and the connect address is enough to tell a
    // network fault from a credential fault.
    if path != "/v1/poll" {
        return;
    }
    let Some(address) = request
        .extensions()
        .get::<axum::extract::ConnectInfo<SocketAddr>>()
        .map(|info| info.0.ip().to_string())
    else {
        return;
    };
    // The node id is in the body, which cannot be read without consuming it, so
    // the id is taken from the query-free peer record by a cheap body peek in
    // the handler instead. Nothing to record here beyond the address.
    let _ = (state, address);
}

async fn hub_status(State(state): State<NodeServerState>) -> Json<Value> {
    Json(state.hub.status().await)
}

enum Peek {
    /// Not a node route: the request is passed through untouched.
    NotNodeRoute(Body),
    /// The body named a node; it is buffered here and replayed to the handler.
    Identified(String, String, Body),
    Oversized,
}

/// Read the node id out of a `/v1/poll` body without losing the body.
///
/// `axum` consumes the body to hand it to the extractor, so the middleware must
/// buffer it. The cap matches the router's own limit: a larger body would be
/// rejected by the handler anyway.
async fn peek_node_id(uri: &axum::http::Uri, body: Body, address: Option<String>) -> Peek {
    const MAX_PEEK: usize = 2 * 1024 * 1024;
    if uri.path() != "/v1/poll" {
        return Peek::NotNodeRoute(body);
    }
    let bytes = match axum::body::to_bytes(body, MAX_PEEK).await {
        Ok(bytes) => bytes,
        Err(_) => return Peek::Oversized,
    };
    let node_id = serde_json::from_slice::<NodePoll>(&bytes).map(|poll| poll.node_id);
    let replay = Body::from(bytes);
    match (node_id, address) {
        (Ok(node_id), Some(address)) => Peek::Identified(node_id, address, replay),
        _ => Peek::NotNodeRoute(replay),
    }
}

async fn register_node(
    State(state): State<NodeServerState>,
    Json(hello): Json<NodeHello>,
) -> Result<Json<RegisterResponse>, NodeApiError> {
    Ok(Json(state.hub.register(hello).await?))
}

async fn poll_node(
    State(state): State<NodeServerState>,
    Json(poll): Json<NodePoll>,
) -> Result<Json<NodePollResponse>, NodeApiError> {
    Ok(Json(state.hub.poll(&poll.node_id).await?))
}

async fn submit_result(
    State(state): State<NodeServerState>,
    Json(result): Json<NodeResult>,
) -> Result<Json<Value>, NodeApiError> {
    state.hub.submit_result(result).await?;
    Ok(Json(json!({"ok": true})))
}

async fn list_nodes(State(state): State<NodeServerState>) -> Json<Value> {
    Json(json!({"nodes": state.hub.list().await}))
}

async fn queue_job(
    State(state): State<NodeServerState>,
    AxumPath(node_id): AxumPath<String>,
    Json(request): Json<QueueJobRequest>,
) -> Result<Json<NodeResult>, NodeApiError> {
    Ok(Json(state.hub.queue_and_wait(&node_id, request).await?))
}

async fn set_policy(
    State(state): State<NodeServerState>,
    AxumPath(node_id): AxumPath<String>,
    Json(request): Json<SetRootPolicyRequest>,
) -> Result<Json<Value>, NodeApiError> {
    state.hub.set_root_policy(&node_id, request.policy).await?;
    Ok(Json(json!({"ok": true, "root_policy": request.policy})))
}

async fn remove_node(
    State(state): State<NodeServerState>,
    AxumPath(node_id): AxumPath<String>,
) -> Result<Json<Value>, NodeApiError> {
    let was_online = state.hub.remove(&node_id).await?;
    Ok(Json(json!({
        "ok": true,
        "node_id": node_id,
        "was_online": was_online,
        // A node still running re-enrolls itself within its reconnect window,
        // so the caller has to stop it there as well.
        "reenrolls_while_running": was_online,
    })))
}

#[derive(Debug)]
struct NodeApiError(anyhow::Error);

impl<E: Into<anyhow::Error>> From<E> for NodeApiError {
    fn from(error: E) -> Self {
        Self(error.into())
    }
}

impl IntoResponse for NodeApiError {
    fn into_response(self) -> Response {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"ok": false, "error": self.0.to_string()})),
        )
            .into_response()
    }
}

#[derive(Clone)]
pub struct NodeClient {
    http: reqwest::Client,
    base_url: String,
    token: String,
}

impl NodeClient {
    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            token: token.into(),
        }
    }

    pub async fn list(&self) -> Result<Value> {
        self.send(self.http.get(format!("{}/v1/nodes", self.base_url)))
            .await
    }

    pub async fn execute(&self, node_id: &str, request: &QueueJobRequest) -> Result<Value> {
        validate_node_id(node_id)?;
        self.send(
            self.http
                .post(format!("{}/v1/nodes/{node_id}/jobs", self.base_url))
                .json(request),
        )
        .await
    }

    pub async fn set_policy(&self, node_id: &str, policy: RootPolicy) -> Result<Value> {
        validate_node_id(node_id)?;
        self.send(
            self.http
                .post(format!("{}/v1/nodes/{node_id}/policy", self.base_url))
                .json(&SetRootPolicyRequest { policy }),
        )
        .await
    }

    pub async fn remove(&self, node_id: &str) -> Result<Value> {
        validate_node_id(node_id)?;
        self.send(
            self.http
                .delete(format!("{}/v1/nodes/{node_id}", self.base_url)),
        )
        .await
    }

    pub async fn status(&self) -> Result<Value> {
        self.send(self.http.get(format!("{}/v1/status", self.base_url)))
            .await
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<Value> {
        let response = request
            .header("X-GnomeAI-Admin-Token", &self.token)
            .send()
            .await
            .context("cannot reach the local GnomeAI node Hub")?;
        let status = response.status();
        let value: Value = response
            .json()
            .await
            .context("node Hub returned invalid JSON")?;
        if !status.is_success() {
            bail!(
                "{}",
                value
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("node Hub request failed")
            )
        }
        Ok(value)
    }
}

pub fn local_client(port: u16, token: &str) -> NodeClient {
    NodeClient::new(format!("http://127.0.0.1:{port}"), token.to_string())
}

fn validate_hello(hello: &NodeHello) -> Result<()> {
    if hello.protocol != NODE_PROTOCOL_VERSION {
        bail!(
            "unsupported node protocol {}; Hub requires {}",
            hello.protocol,
            NODE_PROTOCOL_VERSION
        )
    }
    validate_node_id(&hello.node_id)?;
    if hello.name.trim().is_empty() || hello.name.chars().count() > 128 {
        bail!("node name must contain 1–128 characters")
    }
    if hello.capabilities.len() > 64 {
        bail!("node advertised too many capabilities")
    }
    Ok(())
}

fn validate_node_id(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 96
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!("invalid node id")
    }
    Ok(())
}

fn validate_job_request(request: &QueueJobRequest) -> Result<()> {
    if !matches!(request.action.as_str(), "shell" | "script") {
        bail!("node action must be `shell` or `script`")
    }
    if request.command.chars().count() > 64_000 || request.stdin.chars().count() > 512_000 {
        bail!("node command or script is too large")
    }
    if request.action == "shell" && request.command.trim().is_empty() {
        bail!("shell command is empty")
    }
    if request.action == "script" && request.stdin.trim().is_empty() {
        bail!("script body is empty")
    }
    if let Some(cwd) = &request.cwd
        && (cwd.chars().count() > 4_096 || cwd.chars().any(char::is_control))
    {
        bail!("invalid node working directory")
    }
    Ok(())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Turn "no contact" into something an operator can act on.
///
/// The Hub only knows what it observed, so the sentence names the last thing it
/// saw rather than guessing at a cause it cannot see: a device that was never
/// heard from at all, a node whose credential was just rejected, or a node whose
/// silence began at a moment the person can compare against what they changed.
fn offline_reason(node: &StoredNode, peer: Option<&SeenPeer>, now: u64, enrolled: usize) -> String {
    let mut reason = if node.pending_probe {
        format!(
            "Registered {} but never completed a poll. The node reached this Hub once and then \
             stopped; check that the node process is still running on it.",
            humanize_age(now.saturating_sub(node.last_seen_unix))
        )
    } else {
        format!(
            "No contact for {}. It connected successfully before, so the network path worked at \
             least once.",
            humanize_age(now.saturating_sub(node.last_seen_unix))
        )
    };
    // A node re-registers every 30 s while it is healthy. With more than one
    // node enrolled, silence from exactly one points at that machine; silence
    // from all of them points at this Hub.
    if enrolled > 1 {
        reason.push_str(" Other enrolled nodes are still connecting, so this Hub is reachable.");
    }
    match peer {
        Some(peer) if peer.last_unix >= node.last_seen_unix => {
            reason.push_str(&format!(
                " Its last request came from {} {} and was rejected, so it is running with a \
                 different enrollment token than this Hub expects. Re-enroll it with the current \
                 token.",
                peer.address,
                humanize_age(now.saturating_sub(peer.last_unix))
            ));
        }
        Some(peer) => {
            reason.push_str(&format!(
                " Relay the node serves on {} for its other devices.",
                peer.address
            ));
        }
        None => {
            reason.push_str(
                " No request from this node has reached the Hub since it was last seen, so its \
                 traffic is being dropped before it arrives: check the firewall on this machine, \
                 the Hub bind address, and the server URL in the node's own config.",
            );
        }
    }
    reason
}

/// "23 seconds", "4 minutes", "3 hours", "12 days" — a duration written the way
/// someone would say it out loud.
fn humanize_age(seconds: u64) -> String {
    let (value, unit) = match seconds {
        0..=1 => return "just now".into(),
        2..=59 => (seconds, "second"),
        60..=3_599 => (seconds / 60, "minute"),
        3_600..=86_399 => (seconds / 3_600, "hour"),
        _ => (seconds / 86_400, "day"),
    };
    if value == 1 {
        format!("1 {unit}")
    } else {
        format!("{value} {unit}s")
    }
}

fn cap_text(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        value.to_string()
    } else {
        value.chars().take(max).collect::<String>() + "\n[output truncated]"
    }
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.bytes()
        .zip(right.bytes())
        .fold(0u8, |difference, (a, b)| difference | (a ^ b))
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello(id: &str) -> NodeHello {
        NodeHello {
            protocol: NODE_PROTOCOL_VERSION,
            node_id: id.into(),
            name: id.into(),
            hostname: "host".into(),
            os: "linux".into(),
            arch: "aarch64".into(),
            version: "test".into(),
            init_system: "runit".into(),
            capabilities: vec!["shell".into(), "files".into()],
            root_available: true,
        }
    }

    /// Let a node's silence reach the offline window without waiting it out.
    ///
    /// A freshly registered node is legitimately online for
    /// `NODE_ONLINE_WINDOW_SECS`, so every offline path needs the clock moved
    /// past that window before it can be observed.
    async fn go_silent(hub: &NodeHub, node_id: &str) {
        let mut state = hub.inner.lock().await;
        let node = state
            .nodes
            .get_mut(node_id)
            .expect("the node must be enrolled before it can go silent");
        node.last_seen_unix = unix_now().saturating_sub(NODE_ONLINE_WINDOW_SECS + 10);
    }

    #[tokio::test]
    async fn session_root_grants_do_not_survive_restart() {
        let root = std::env::temp_dir().join(format!(
            "gnomeai-node-hub-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("nodes.json");
        let hub = NodeHub::open(path.clone()).unwrap();
        hub.register(hello("pi")).await.unwrap();
        hub.set_root_policy("pi", RootPolicy::Session)
            .await
            .unwrap();
        drop(hub);
        let reopened = NodeHub::open(path).unwrap();
        assert_eq!(reopened.list().await[0].root_policy, RootPolicy::Ask);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn disabled_root_is_rejected_before_queueing() {
        let root = std::env::temp_dir().join(format!(
            "gnomeai-node-hub-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let hub = NodeHub::open(root.join("nodes.json")).unwrap();
        hub.register(hello("pi")).await.unwrap();
        hub.set_root_policy("pi", RootPolicy::Disabled)
            .await
            .unwrap();
        let error = hub
            .queue_and_wait(
                "pi",
                QueueJobRequest {
                    action: "shell".into(),
                    command: "id".into(),
                    stdin: String::new(),
                    cwd: None,
                    timeout_secs: 1,
                    root: true,
                    root_approved: true,
                },
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("root is disabled"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn an_offline_node_explains_itself() {
        let root = std::env::temp_dir().join(format!(
            "gnomeai-node-hub-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let hub = NodeHub::open(root.join("nodes.json")).unwrap();
        hub.register(hello("pi")).await.unwrap();
        go_silent(&hub, "pi").await;
        // Registered but never polled: the node reached the Hub and stopped.
        let record = &hub.list().await[0];
        assert!(!record.online);
        assert!(record.offline_reason.contains("never completed a poll"));

        // A request that arrives while the node is otherwise silent means the
        // traffic is getting through, so the cause is not the network path.
        hub.note_seen_peer("pi", "192.168.1.178").await;
        let record = &hub.list().await[0];
        assert!(record.offline_reason.contains("192.168.1.178"));
        assert!(record.offline_reason.contains("enrollment token"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn removing_a_node_forgets_it_and_its_queue() {
        let root = std::env::temp_dir().join(format!(
            "gnomeai-node-hub-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let hub = NodeHub::open(root.join("nodes.json")).unwrap();
        hub.register(hello("pi")).await.unwrap();
        go_silent(&hub, "pi").await;
        assert!(!hub.remove("pi").await.unwrap());
        assert!(hub.list().await.is_empty());
        // A second removal has nothing left to remove.
        assert!(hub.remove("pi").await.is_err());
        // The enrollment does not come back on restart.
        drop(hub);
        let reopened = NodeHub::open(root.join("nodes.json")).unwrap();
        assert!(reopened.list().await.is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Reserve a port the Hub can bind, so the test does not collide with a
    /// development instance already listening on the default one.
    fn free_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    }

    #[tokio::test]
    async fn a_rejected_node_is_still_identified_over_http() {
        const NODE_TOKEN: &str = "0123456789abcdef0123456789abcdef";
        const ADMIN_TOKEN: &str = "fedcba9876543210fedcba9876543210";
        let root = std::env::temp_dir().join(format!(
            "gnomeai-node-hub-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let hub = NodeHub::open(root.join("nodes.json")).unwrap();
        let port = free_port();
        let served = hub.clone();
        tokio::spawn(async move {
            let _ = serve(
                served,
                SocketAddr::from(([127, 0, 0, 1], port)),
                NODE_TOKEN.into(),
                ADMIN_TOKEN.into(),
            )
            .await;
        });

        let http = reqwest::Client::new();
        let base = format!("http://127.0.0.1:{port}");
        // The listener is spawned, not awaited, so poll until it accepts.
        let mut ready = false;
        for _ in 0..100 {
            if http
                .get(format!("{base}/v1/status"))
                .header("X-GnomeAI-Admin-Token", ADMIN_TOKEN)
                .send()
                .await
                .is_ok()
            {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(ready, "the Hub did not start listening");

        // Enroll with the node token.
        let response = http
            .post(format!("{base}/v1/register"))
            .header("X-GnomeAI-Node-Token", NODE_TOKEN)
            .json(&hello("pi"))
            .send()
            .await
            .unwrap();
        assert!(response.status().is_success());

        // The admin credential is what lists nodes; the node credential is not.
        let forbidden = http
            .get(format!("{base}/v1/nodes"))
            .header("X-GnomeAI-Node-Token", NODE_TOKEN)
            .send()
            .await
            .unwrap();
        assert_eq!(forbidden.status(), StatusCode::UNAUTHORIZED);

        // A poll with the wrong token is rejected, but the Hub still records who
        // asked, which is the difference between "offline" and "wrong token".
        let rejected = http
            .post(format!("{base}/v1/poll"))
            .header("X-GnomeAI-Node-Token", "ffffffffffffffffffffffffffffffff")
            .json(&NodePoll {
                node_id: "pi".into(),
            })
            .send()
            .await
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::UNAUTHORIZED);
        go_silent(&hub, "pi").await;

        let listed: Value = http
            .get(format!("{base}/v1/nodes"))
            .header("X-GnomeAI-Admin-Token", ADMIN_TOKEN)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let reason = listed["nodes"][0]["offline_reason"].as_str().unwrap_or("");
        assert!(reason.contains("127.0.0.1"), "unexpected reason: {reason}");
        assert!(
            reason.contains("enrollment token"),
            "unexpected reason: {reason}"
        );

        // Removing it over HTTP forgets the enrollment.
        let removed = http
            .delete(format!("{base}/v1/nodes/pi"))
            .header("X-GnomeAI-Admin-Token", ADMIN_TOKEN)
            .send()
            .await
            .unwrap();
        assert!(removed.status().is_success());
        let listed: Value = http
            .get(format!("{base}/v1/nodes"))
            .header("X-GnomeAI-Admin-Token", ADMIN_TOKEN)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(listed["nodes"].as_array().unwrap().is_empty());
        drop(hub);
        std::fs::remove_dir_all(root).unwrap();
    }
}
