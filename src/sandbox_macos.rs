//! macOS execution backend for GnomeAI-RS.
//!
//! Linux keeps the Landlock + seccomp implementation in sandbox.rs.  On macOS
//! normal/full-access commands use the user's native permissions, while strict
//! read-only/internal jobs are wrapped in Apple's sandbox-exec when available.
//! Timeout, cancellation and bounded output semantics match the Linux backend.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
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

impl SandboxPolicy {
    pub fn isolated_workspace_write(cwd: impl Into<PathBuf>) -> Self {
        let cwd = cwd.into();
        let home = dirs_home();
        let mut writable = vec![
            cwd.clone(),
            PathBuf::from("/tmp"),
            PathBuf::from("/private/tmp"),
        ];
        if let Some(h) = &home {
            writable.push(h.join(".cargo/registry"));
            writable.push(h.join(".cargo/git"));
            writable.push(h.join(".cache"));
            writable.push(h.join(".npm"));
            writable.push(h.join("Library/Caches"));
        }
        let mut readable = vec![
            PathBuf::from("/System"),
            PathBuf::from("/Library"),
            PathBuf::from("/usr"),
            PathBuf::from("/bin"),
            PathBuf::from("/sbin"),
            PathBuf::from("/private/etc"),
            PathBuf::from("/dev"),
        ];
        if let Some(h) = &home {
            readable.push(h.join(".rustup"));
        }
        Self {
            mode: SandboxMode::IsolatedWorkspaceWrite,
            cwd,
            writable,
            readable,
            allow_network: false,
            allow_privilege_escalation: false,
            require_landlock: false,
            timeout_ms: 120_000,
            max_output_bytes: 4 * 1024 * 1024,
            env_allowlist: vec![
                "PATH".into(),
                "HOME".into(),
                "LANG".into(),
                "LC_ALL".into(),
                "TERM".into(),
                "TMPDIR".into(),
                "CARGO_HOME".into(),
                "RUSTUP_HOME".into(),
                "CARGO_TARGET_DIR".into(),
            ],
            env_extra: Vec::new(),
        }
    }

    pub fn read_only(cwd: impl Into<PathBuf>) -> Self {
        let mut policy = Self::isolated_workspace_write(cwd);
        policy.mode = SandboxMode::ReadOnly;
        policy.readable.push(policy.cwd.clone());
        policy.writable = vec![PathBuf::from("/tmp"), PathBuf::from("/private/tmp")];
        policy
    }

    pub fn normal(cwd: impl Into<PathBuf>) -> Self {
        Self::unrestricted(cwd, SandboxMode::Normal)
    }

    pub fn full_access(cwd: impl Into<PathBuf>) -> Self {
        Self::unrestricted(cwd, SandboxMode::FullAccess)
    }

    fn unrestricted(cwd: impl Into<PathBuf>, mode: SandboxMode) -> Self {
        Self {
            mode,
            cwd: cwd.into(),
            writable: Vec::new(),
            readable: Vec::new(),
            allow_network: true,
            allow_privilege_escalation: false,
            require_landlock: false,
            timeout_ms: 120_000,
            max_output_bytes: 4 * 1024 * 1024,
            env_allowlist: Vec::new(),
            env_extra: Vec::new(),
        }
    }

    fn inherits_environment(&self) -> bool {
        matches!(self.mode, SandboxMode::Normal | SandboxMode::FullAccess)
    }
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Linux uses a re-exec helper. macOS builds execute directly or through
/// sandbox-exec, so there is no helper mode to enter.
pub fn maybe_run_as_helper() -> Result<()> {
    Ok(())
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

pub async fn spawn_sandboxed(
    policy: &SandboxPolicy,
    program: &str,
    args: &[String],
) -> Result<ExecOutput> {
    spawn_sandboxed_with_cancel(policy, program, args, &CancellationToken::new()).await
}

pub async fn spawn_sandboxed_with_cancel(
    policy: &SandboxPolicy,
    program: &str,
    args: &[String],
    cancel: &CancellationToken,
) -> Result<ExecOutput> {
    let mut cmd = sandboxed_command(policy, program, args)?;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn().context("failed to spawn sandboxed command")?;
    let pid = child.id().context("child has no pid")? as i32;
    let mut stdout_pipe = child.stdout.take().context("no stdout pipe")?;
    let mut stderr_pipe = child.stderr.take().context("no stderr pipe")?;
    let cap = policy.max_output_bytes;
    let out_task = tokio::spawn(async move { read_capped(&mut stdout_pipe, cap).await });
    let err_task = tokio::spawn(async move { read_capped(&mut stderr_pipe, cap).await });

    let mut timed_out = false;
    let mut cancelled = false;
    let status = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            cancelled = true;
            terminate_group(pid).await;
            child.wait().await?
        }
        result = tokio::time::timeout(Duration::from_millis(policy.timeout_ms), child.wait()) => {
            match result {
                Ok(status) => status?,
                Err(_) => {
                    timed_out = true;
                    terminate_group(pid).await;
                    child.wait().await?
                }
            }
        }
    };

    let (stdout, out_trunc) = out_task.await??;
    let (stderr, err_trunc) = err_task.await??;
    Ok(ExecOutput {
        exit_code: status.code(),
        stdout,
        stderr,
        timed_out,
        cancelled,
        truncated: out_trunc || err_trunc,
    })
}

pub fn sandboxed_command(
    policy: &SandboxPolicy,
    program: &str,
    args: &[String],
) -> Result<Command> {
    let strict = matches!(
        policy.mode,
        SandboxMode::ReadOnly | SandboxMode::IsolatedWorkspaceWrite
    );
    let mut cmd = if strict {
        let sandbox_exec = Path::new("/usr/bin/sandbox-exec");
        if !sandbox_exec.is_file() {
            bail!(
                "strict sandboxing on macOS requires /usr/bin/sandbox-exec; \
                 use normal/full-access for commands you explicitly trust"
            );
        }
        let mut command = Command::new(sandbox_exec);
        command.arg("-p").arg(build_profile(policy));
        command.arg(program).args(args);
        command
    } else {
        let mut command = Command::new(program);
        command.args(args);
        command
    };

    if !policy.inherits_environment() {
        cmd.env_clear();
        for key in &policy.env_allowlist {
            if let Some(value) = std::env::var_os(key) {
                cmd.env(key, value);
            }
        }
    }
    for (key, value) in &policy.env_extra {
        cmd.env(key, value);
    }
    cmd.current_dir(&policy.cwd);
    // Give every command its own process group so timeout/interrupt reaches
    // descendants such as cargo/rustc, npm/node and shell pipelines.
    cmd.as_std_mut().process_group(0);
    Ok(cmd)
}

fn build_profile(policy: &SandboxPolicy) -> String {
    let mut profile = String::from(
        "(version 1)\n\
         (deny default)\n\
         (allow process*)\n\
         (allow sysctl-read)\n\
         (allow mach-lookup)\n\
         (allow signal (target same-sandbox))\n\
         (allow file-read-metadata)\n",
    );

    for path in &policy.readable {
        profile.push_str(&format!(
            "(allow file-read* (subpath {}))\n",
            sbpl_string(path)
        ));
    }
    for path in &policy.writable {
        // Writes also need reads for compilers, editors and atomic renames.
        profile.push_str(&format!(
            "(allow file-read* file-write* (subpath {}))\n",
            sbpl_string(path)
        ));
    }
    for device in ["/dev/null", "/dev/zero", "/dev/random", "/dev/urandom"] {
        profile.push_str(&format!(
            "(allow file-read* file-write* (literal \"{device}\"))\n"
        ));
    }
    if policy.allow_network {
        profile.push_str("(allow network*)\n");
    }
    profile
}

fn sbpl_string(path: &Path) -> String {
    let value = path.to_string_lossy();
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

async fn terminate_group(pid: i32) {
    unsafe { libc::killpg(pid, libc::SIGTERM) };
    tokio::time::sleep(Duration::from_millis(250)).await;
    unsafe { libc::killpg(pid, libc::SIGKILL) };
}

async fn read_capped<R>(reader: &mut R, cap: usize) -> Result<(String, bool)>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buf = Vec::with_capacity(cap.min(8192));
    let mut chunk = [0u8; 8192];
    let mut total = 0usize;
    let mut truncated = false;
    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        total += n;
        if buf.len() < cap {
            let take = n.min(cap - buf.len());
            buf.extend_from_slice(&chunk[..take]);
            if take < n {
                truncated = true;
            }
        } else {
            truncated = true;
        }
    }
    let mut text = String::from_utf8_lossy(&buf).into_owned();
    if truncated {
        text.push_str(&format!(
            "\n\n[... output truncated, {total} bytes total ...]"
        ));
    }
    Ok((text, truncated))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_profile_contains_workspace_and_blocks_network_by_default() {
        let policy = SandboxPolicy::isolated_workspace_write("/tmp/gnome-ai-test");
        let profile = build_profile(&policy);
        assert!(profile.contains("/tmp/gnome-ai-test"));
        assert!(!profile.contains("(allow network*)"));
    }

    #[test]
    fn normal_policy_keeps_network_and_user_access() {
        let policy = SandboxPolicy::normal("/tmp");
        assert!(policy.allow_network);
        assert!(policy.inherits_environment());
    }
}
