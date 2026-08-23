//! Shared, init-system-agnostic wire protocol between a GnomeAI Hub and nodes.

use serde::{Deserialize, Serialize};

pub const NODE_PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootPolicy {
    Disabled,
    Ask,
    Session,
    Always,
}

impl Default for RootPolicy {
    fn default() -> Self {
        Self::Ask
    }
}

impl RootPolicy {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "disabled" | "off" | "blocked" => Some(Self::Disabled),
            "ask" | "confirm" => Some(Self::Ask),
            "session" | "temporary" => Some(Self::Session),
            "always" | "permanent" => Some(Self::Always),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeHello {
    pub protocol: u32,
    pub node_id: String,
    pub name: String,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub version: String,
    #[serde(default)]
    pub init_system: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub root_available: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeRecord {
    #[serde(flatten)]
    pub hello: NodeHello,
    pub last_seen_unix: u64,
    pub online: bool,
    #[serde(default)]
    pub root_policy: RootPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeJob {
    pub job_id: String,
    pub action: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub stdin: String,
    #[serde(default)]
    pub cwd: Option<String>,
    pub timeout_secs: u64,
    #[serde(default)]
    pub root: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodePoll {
    pub node_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodePollResponse {
    pub job: Option<NodeJob>,
    pub root_policy: RootPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeResult {
    pub node_id: String,
    pub job_id: String,
    pub ok: bool,
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueJobRequest {
    pub action: String,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub stdin: String,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    #[serde(default)]
    pub root: bool,
    /// Set only by the trusted Hub-side tool after the central policy/approval
    /// flow has authorized this specific privileged request.
    #[serde(default)]
    pub root_approved: bool,
}

fn default_timeout() -> u64 {
    60
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetRootPolicyRequest {
    pub policy: RootPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterResponse {
    pub ok: bool,
    pub root_policy: RootPolicy,
    pub poll_after_ms: u64,
}
