//! Android relies on the application sandbox; no process tools are registered.
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    ReadOnly,
    #[serde(alias = "workspace-write")]
    Normal,
    #[serde(alias = "danger-full-access")]
    FullAccess,
    IsolatedWorkspaceWrite,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxPolicy {
    pub mode: SandboxMode,
    pub cwd: PathBuf,
    pub writable: Vec<PathBuf>,
    pub readable: Vec<PathBuf>,
    pub allow_network: bool,
    pub allow_privilege_escalation: bool,
    #[serde(default)]
    pub require_landlock: bool,
    pub timeout_ms: u64,
    pub max_output_bytes: usize,
    pub env_allowlist: Vec<String>,
    #[serde(default)]
    pub env_extra: Vec<(String, String)>,
}

#[derive(Debug)]
pub struct ExecOutput {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    pub cancelled: bool,
    pub truncated: bool,
}

impl SandboxPolicy {
    pub fn normal(cwd: impl Into<PathBuf>) -> Self {
        let cwd = cwd.into();
        Self {
            mode: SandboxMode::Normal,
            writable: vec![cwd.clone()],
            readable: vec![cwd.clone()],
            cwd,
            allow_network: true,
            allow_privilege_escalation: false,
            require_landlock: false,
            timeout_ms: 120000,
            max_output_bytes: 16384,
            env_allowlist: vec![],
            env_extra: vec![],
        }
    }
    pub fn read_only(cwd: impl Into<PathBuf>) -> Self {
        let mut p = Self::normal(cwd);
        p.mode = SandboxMode::ReadOnly;
        p.writable.clear();
        p
    }
    pub fn full_access(cwd: impl Into<PathBuf>) -> Self {
        Self::normal(cwd)
    }
    pub fn isolated_workspace_write(cwd: impl Into<PathBuf>) -> Self {
        Self::normal(cwd)
    }
}
pub fn maybe_run_as_helper() -> Result<()> {
    Ok(())
}
pub fn sandboxed_command(
    _: &SandboxPolicy,
    _: &str,
    _: &[String],
) -> Result<tokio::process::Command> {
    bail!("Process tools are not part of the Android runtime")
}
pub async fn spawn_sandboxed(
    policy: &SandboxPolicy,
    program: &str,
    args: &[String],
) -> Result<ExecOutput> {
    spawn_sandboxed_with_cancel(policy, program, args, &CancellationToken::new()).await
}
pub async fn spawn_sandboxed_with_cancel(
    _: &SandboxPolicy,
    _: &str,
    _: &[String],
    _: &CancellationToken,
) -> Result<ExecOutput> {
    bail!("Process tools are not part of the Android runtime")
}
