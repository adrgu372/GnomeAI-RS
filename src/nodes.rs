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
        NODE_PROTOCOL_VERSION, NodeHello, NodeJob, NodePoll, NodePollResponse, NodeRecord,
        NodeResult, QueueJobRequest, RegisterResponse, RootPolicy, SetRootPolicyRequest,
    },
    storage::write_private,
};

const ONLINE_WINDOW_SECS: u64 = 45;
const LONG_POLL_SECS: u64 = 25;
const MAX_RESULT_CHARS: usize = 1_000_000;

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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredNode {
    hello: NodeHello,
    last_seen_unix: u64,
    #[serde(default)]
    root_policy: RootPolicy,
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
        self.inner
            .lock()
            .await
            .nodes
            .values()
            .map(|node| NodeRecord {
                hello: node.hello.clone(),
                last_seen_unix: node.last_seen_unix,
                online: now.saturating_sub(node.last_seen_unix) <= ONLINE_WINDOW_SECS,
                root_policy: node.root_policy,
            })
            .collect()
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
            if unix_now().saturating_sub(last_seen_unix) > ONLINE_WINDOW_SECS {
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
        .route("/v1/nodes/{node_id}/jobs", post(queue_job))
        .route("/v1/nodes/{node_id}/policy", post(set_policy))
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            protect_node_api,
        ))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!(%bind, "GnomeAI node Hub listening");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn protect_node_api(
    State(state): State<NodeServerState>,
    request: Request<Body>,
    next: Next,
) -> Response {
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
}
