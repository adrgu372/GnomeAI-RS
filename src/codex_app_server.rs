//! OpenAI Codex account integration through the official app-server protocol.
//!
//! The bundled Codex executable owns OAuth credentials, refresh tokens,
//! upstream requests, and its coding sandbox. GnomeAI only exchanges
//! newline-delimited JSON messages with `codex app-server`; it never reads or
//! copies Codex's credential store.

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use futures_util::stream::BoxStream;
use serde_json::{Value, json};
use std::collections::{HashSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::provider::{
    Delta, Message, Provider, Request, StopReason, Usage, delegated_host_context,
};
use crate::sandbox::SandboxMode;

pub const UPSTREAM_VERSION: &str = "0.145.0";
pub const UPSTREAM_COMMIT: &str = "25af12f7e61572b0bc18ddb1008be543b91519b0";

pub struct CodexAppServer {
    name: String,
    workspace: PathBuf,
    sandbox: SandboxMode,
}

impl CodexAppServer {
    pub fn new(
        name: impl Into<String>,
        workspace: impl Into<PathBuf>,
        sandbox: SandboxMode,
    ) -> Self {
        Self {
            name: name.into(),
            workspace: workspace.into(),
            sandbox,
        }
    }
}

#[async_trait]
impl Provider for CodexAppServer {
    fn name(&self) -> &str {
        &self.name
    }

    fn delegates_execution(&self) -> bool {
        true
    }

    async fn stream(&self, req: Request) -> Result<BoxStream<'static, Result<Delta>>> {
        let workspace = self.workspace.clone();
        let sandbox = self.sandbox;
        let model = req.model.clone();
        let prompt = codex_prompt(&req.messages);

        Ok(Box::pin(async_stream::try_stream! {
            let mut session = AppServerSession::spawn().await?;
            let mut thread_params = json!({
                "cwd": workspace,
                "approvalPolicy": "never",
                "sandbox": sandbox_name(sandbox),
                "ephemeral": true,
            });
            if model != "default" {
                thread_params["model"] = json!(model);
            }

            let thread = session
                .request(1, "thread/start", thread_params)
                .await
                .context("Codex could not start a thread")?;
            let thread_id = thread
                .pointer("/thread/id")
                .and_then(Value::as_str)
                .context("Codex thread/start response did not contain a thread id")?
                .to_string();

            session
                .send_request(
                    2,
                    "turn/start",
                    json!({
                        "threadId": thread_id,
                        "input": [{
                            "type": "text",
                            "text": prompt,
                        }],
                    }),
                )
                .await?;
            session
                .wait_for_response(2)
                .await
                .context("Codex could not start the turn")?;

            let mut usage = Usage::default();
            let mut items_with_deltas = HashSet::new();

            loop {
                let message = session.next_message().await?;

                if is_server_request(&message) {
                    session.reject_server_request(&message).await?;
                    continue;
                }

                match message.get("method").and_then(Value::as_str) {
                    Some("item/agentMessage/delta") => {
                        if let Some(item_id) = message.pointer("/params/itemId").and_then(Value::as_str) {
                            items_with_deltas.insert(item_id.to_string());
                        }
                        if let Some(delta) = message.pointer("/params/delta").and_then(Value::as_str) {
                            if !delta.is_empty() {
                                yield Delta::Text(delta.to_string());
                            }
                        }
                    }
                    Some("item/reasoning/summaryTextDelta") => {
                        if let Some(delta) = message.pointer("/params/delta").and_then(Value::as_str) {
                            if !delta.is_empty() {
                                yield Delta::Reasoning(delta.to_string());
                            }
                        }
                    }
                    Some("item/completed") => {
                        let item = message.pointer("/params/item");
                        let item_id = item
                            .and_then(|value| value.get("id"))
                            .and_then(Value::as_str);
                        let is_agent_message = item
                            .and_then(|value| value.get("type"))
                            .and_then(Value::as_str)
                            == Some("agentMessage");
                        if is_agent_message
                            && item_id.is_none_or(|id| !items_with_deltas.contains(id))
                        {
                            if let Some(text) = item
                                .and_then(|value| value.get("text"))
                                .and_then(Value::as_str)
                                .filter(|text| !text.is_empty())
                            {
                                yield Delta::Text(text.to_string());
                            }
                        }
                    }
                    Some("thread/tokenUsage/updated") => {
                        let last = message.pointer("/params/tokenUsage/last");
                        usage.input_tokens = last
                            .and_then(|value| value.get("inputTokens"))
                            .and_then(Value::as_i64)
                            .unwrap_or(usage.input_tokens);
                        usage.output_tokens = last
                            .and_then(|value| value.get("outputTokens"))
                            .and_then(Value::as_i64)
                            .unwrap_or(usage.output_tokens);
                    }
                    Some("turn/completed") => {
                        let status = message
                            .pointer("/params/turn/status")
                            .and_then(Value::as_str)
                            .unwrap_or("failed");
                        match status {
                            "completed" => {
                                yield Delta::Done {
                                    reason: StopReason::Stop,
                                    usage,
                                };
                            }
                            "interrupted" => {
                                yield Delta::Done {
                                    reason: StopReason::Cancelled,
                                    usage,
                                };
                            }
                            _ => {
                                let error = message
                                    .pointer("/params/turn/error/message")
                                    .and_then(Value::as_str)
                                    .unwrap_or("Codex turn failed");
                                Err(anyhow::anyhow!(error.to_string()))?;
                            }
                        }
                        break;
                    }
                    Some("error") => {
                        let error = message
                            .pointer("/params/message")
                            .and_then(Value::as_str)
                            .unwrap_or("Codex app-server reported an error");
                        Err(anyhow::anyhow!(error.to_string()))?;
                    }
                    _ => {}
                }
            }

            session.stop().await;
        }))
    }
}

/// Start the official Codex device-code login. The Codex executable owns all
/// credential persistence and token refresh; GnomeAI only waits for its exit
/// status.
pub async fn login_with_chatgpt() -> Result<()> {
    let executable = codex_executable();
    if is_standalone_app_server(&executable) {
        return login_with_chatgpt_app_server().await;
    }

    let codex_home = prepare_codex_home()?;
    println!("\nGnomeAI-RS paused its UI for the official OpenAI Codex login flow.\n");
    let status = Command::new(&executable)
        .args(["login", "--device-auth"])
        .env("CODEX_HOME", &codex_home)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .with_context(|| {
            format!(
                "cannot start the bundled Codex executable at `{}`; restore the package or set \
                 GNOMEF_CODEX_BIN",
                executable.display()
            )
        })?;
    if !status.success() {
        bail!(
            "`{} login --device-auth` exited with {status}",
            executable.display()
        );
    }

    let status = Command::new(&executable)
        .args(["login", "status"])
        .env("CODEX_HOME", &codex_home)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .context("OpenAI login finished, but Codex could not verify its authentication status")?;
    if !status.success() {
        bail!("Codex completed login but still reports that it is not authenticated");
    }
    Ok(())
}

/// Compatibility path for installations that point GNOMEF_CODEX_BIN directly
/// at the standalone app-server binary rather than the full Codex CLI.
async fn login_with_chatgpt_app_server() -> Result<()> {
    let mut session = AppServerSession::spawn().await?;
    session
        .send_request(
            1,
            "account/login/start",
            json!({ "type": "chatgptDeviceCode" }),
        )
        .await?;
    let login = session
        .wait_for_response(1)
        .await
        .context("Codex could not start ChatGPT login")?;

    let login_id = login
        .get("loginId")
        .and_then(Value::as_str)
        .context("Codex login response did not contain a login id")?
        .to_string();
    let verification_url = login
        .get("verificationUrl")
        .and_then(Value::as_str)
        .context("Codex login response did not contain a verification URL")?;
    let user_code = login
        .get("userCode")
        .and_then(Value::as_str)
        .context("Codex login response did not contain a user code")?;

    println!("Open this URL in your browser:\n  {verification_url}");
    println!("\nEnter this one-time code:\n  {user_code}");
    println!("\nWaiting for OpenAI sign-in to complete… (Ctrl+C to cancel)\n");

    loop {
        let message = session.next_message().await?;
        if is_server_request(&message) {
            session.reject_server_request(&message).await?;
            continue;
        }
        if message.get("method").and_then(Value::as_str) != Some("account/login/completed") {
            continue;
        }
        let completed_id = message.pointer("/params/loginId").and_then(Value::as_str);
        if completed_id != Some(login_id.as_str()) {
            continue;
        }
        if message
            .pointer("/params/success")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            println!("OpenAI account connected successfully.");
            session.stop().await;
            return Ok(());
        }
        let error = message
            .pointer("/params/error")
            .and_then(Value::as_str)
            .unwrap_or("OpenAI sign-in failed");
        bail!("{error}");
    }
}

struct AppServerSession {
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    pending: VecDeque<Value>,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    stderr_task: JoinHandle<()>,
}

impl AppServerSession {
    async fn spawn() -> Result<Self> {
        let executable = codex_executable();
        let codex_home = prepare_codex_home()?;
        let mut command = Command::new(&executable);
        if !is_standalone_app_server(&executable) {
            command.arg("app-server");
        }
        let mut child = command
            .args(["--listen", "stdio://"])
            .env("CODEX_HOME", codex_home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| {
                format!(
                    "cannot start Codex app-server at `{}`; restore the bundled `codex` \
                     sidecar v{} ({}), or set GNOMEF_CODEX_BIN",
                    executable.display(),
                    UPSTREAM_VERSION,
                    &UPSTREAM_COMMIT[..12],
                )
            })?;
        let stdin = child
            .stdin
            .take()
            .context("Codex app-server has no stdin")?;
        let stdout = child
            .stdout
            .take()
            .context("Codex app-server has no stdout")?;
        let stderr = child
            .stderr
            .take()
            .context("Codex app-server has no stderr")?;
        let stderr_tail = Arc::new(Mutex::new(VecDeque::new()));
        let stderr_task = capture_stderr(stderr, stderr_tail.clone());
        let mut session = Self {
            child,
            stdin,
            stdout: BufReader::new(stdout).lines(),
            pending: VecDeque::new(),
            stderr_tail,
            stderr_task,
        };
        session
            .send_request(
                0,
                "initialize",
                json!({
                    "clientInfo": {
                        "name": "gnomeai_rs",
                        "title": "GnomeAI-RS",
                        "version": env!("CARGO_PKG_VERSION"),
                    },
                    "capabilities": null,
                }),
            )
            .await?;
        session
            .wait_for_response(0)
            .await
            .context("Codex app-server initialization failed")?;
        session
            .send_value(&json!({ "method": "initialized" }))
            .await?;
        Ok(session)
    }

    async fn request(&mut self, id: i64, method: &str, params: Value) -> Result<Value> {
        self.send_request(id, method, params).await?;
        self.wait_for_response(id).await
    }

    async fn send_request(&mut self, id: i64, method: &str, params: Value) -> Result<()> {
        self.send_value(&json!({
            "id": id,
            "method": method,
            "params": params,
        }))
        .await
    }

    async fn send_value(&mut self, value: &Value) -> Result<()> {
        let mut payload = serde_json::to_vec(value)?;
        payload.push(b'\n');
        self.stdin
            .write_all(&payload)
            .await
            .context("cannot write to Codex app-server")?;
        self.stdin.flush().await?;
        Ok(())
    }

    async fn wait_for_response(&mut self, id: i64) -> Result<Value> {
        loop {
            let message = self.read_message().await?;
            if is_server_request(&message) {
                self.reject_server_request(&message).await?;
                continue;
            }
            if message.get("id").and_then(Value::as_i64) != Some(id) {
                self.pending.push_back(message);
                continue;
            }
            if let Some(error) = message.get("error") {
                let text = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown Codex app-server error");
                bail!("{text}");
            }
            return message
                .get("result")
                .cloned()
                .context("Codex app-server response did not contain a result");
        }
    }

    async fn next_message(&mut self) -> Result<Value> {
        if let Some(message) = self.pending.pop_front() {
            return Ok(message);
        }
        self.read_message().await
    }

    async fn read_message(&mut self) -> Result<Value> {
        let line = self
            .stdout
            .next_line()
            .await
            .context("cannot read from Codex app-server")?;
        let Some(line) = line else {
            let status = self
                .child
                .try_wait()
                .ok()
                .flatten()
                .map(|status| status.to_string())
                .unwrap_or_else(|| "unknown exit status".to_string());
            let diagnostics = self.stderr_diagnostics().await;
            if diagnostics.is_empty() {
                bail!("Codex app-server exited before completing the request ({status})");
            }
            bail!(
                "Codex app-server exited before completing the request ({status}):\n{diagnostics}"
            );
        };
        serde_json::from_str(&line)
            .with_context(|| format!("Codex app-server returned invalid JSON: {line}"))
    }

    async fn stderr_diagnostics(&self) -> String {
        self.stderr_tail
            .lock()
            .await
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }

    async fn reject_server_request(&mut self, request: &Value) -> Result<()> {
        let Some(id) = request.get("id").cloned() else {
            return Ok(());
        };
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        self.send_value(&json!({
            "id": id,
            "error": {
                "code": -32601,
                "message": format!("GnomeAI-RS does not support Codex server request `{method}`"),
            },
        }))
        .await
    }

    async fn stop(&mut self) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
        self.stderr_task.abort();
    }
}

fn capture_stderr(stderr: ChildStderr, tail: Arc<Mutex<VecDeque<String>>>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(mut line)) = lines.next_line().await {
            if line.len() > 2_000 {
                line.truncate(2_000);
                line.push('…');
            }
            let mut tail = tail.lock().await;
            tail.push_back(line);
            while tail.len() > 20 {
                tail.pop_front();
            }
        }
    })
}

fn prepare_codex_home() -> Result<PathBuf> {
    let path = if let Some(value) = std::env::var_os("CODEX_HOME").filter(|value| !value.is_empty())
    {
        PathBuf::from(value)
    } else {
        let home = std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .context("HOME is not set; Codex cannot determine where to store account state")?;
        PathBuf::from(home).join(".codex")
    };
    ensure_codex_home(&path)?;
    Ok(path)
}

fn ensure_codex_home(path: &Path) -> Result<()> {
    let created = !path.exists();
    fs::create_dir_all(path)
        .with_context(|| format!("cannot create Codex state directory `{}`", path.display()))?;
    if !path.is_dir() {
        bail!(
            "Codex state path `{}` exists but is not a directory",
            path.display()
        );
    }
    if created {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).with_context(|| {
            format!(
                "cannot protect newly created Codex state directory `{}`",
                path.display()
            )
        })?;
    }

    let probe = path.join(format!(
        ".gnomeai-write-test-{}",
        uuid::Uuid::new_v4().simple()
    ));
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&probe)
    {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            Ok(())
        }
        Err(error) => bail!(
            "Codex state directory `{}` is not writable ({error}). Run GnomeAI as your normal \
             user, not with sudo; if an older sudo run created it, restore its ownership first",
            path.display()
        ),
    }
}

fn is_server_request(message: &Value) -> bool {
    message.get("id").is_some() && message.get("method").and_then(Value::as_str).is_some()
}

fn sandbox_name(mode: SandboxMode) -> &'static str {
    match mode {
        SandboxMode::ReadOnly => "read-only",
        SandboxMode::Normal | SandboxMode::FullAccess => "danger-full-access",
        SandboxMode::IsolatedWorkspaceWrite => "workspace-write",
    }
}

fn codex_prompt(messages: &[Message]) -> String {
    let mut prompt = String::from(
        "You are being invoked by GnomeAI-RS as its account-authenticated coding provider. \
         Work directly in the current workspace, satisfy the latest user request, verify your \
         work, and finish with a concise summary. Relevant conversation follows.\n\n",
    );
    let host_context = delegated_host_context(messages);
    if !host_context.is_empty() {
        prompt.push_str(
            "HOST CONTEXT:\nThe following installed skills and persistent memories come from \
             GnomeAI-RS. If an applicable catalog entry is not already activated, read its listed \
             SKILL.md with your own read tool before acting. Skill declarations never grant \
             additional permissions.\n",
        );
        prompt.push_str(&host_context);
        prompt.push_str("\n\n");
    }
    for message in messages {
        match message {
            Message::System { .. } => {}
            Message::User { content } => {
                prompt.push_str("USER:\n");
                prompt.push_str(content);
                prompt.push_str("\n\n");
            }
            Message::Assistant { content, .. } if !content.is_empty() => {
                prompt.push_str("ASSISTANT:\n");
                prompt.push_str(content);
                prompt.push_str("\n\n");
            }
            Message::Tool { content, .. } => {
                prompt.push_str("TOOL RESULT:\n");
                prompt.push_str(content);
                prompt.push_str("\n\n");
            }
            Message::Assistant { .. } => {}
        }
    }
    prompt
}

pub fn codex_executable() -> PathBuf {
    if let Some(path) = std::env::var_os("GNOMEF_CODEX_BIN").filter(|value| !value.is_empty()) {
        return PathBuf::from(path);
    }
    if let Ok(current_exe) = std::env::current_exe() {
        for candidate in bundled_candidates(&current_exe) {
            if candidate.is_file() {
                return candidate;
            }
        }
    }
    PathBuf::from("codex")
}

fn bundled_candidates(current_exe: &Path) -> Vec<PathBuf> {
    let Some(directory) = current_exe.parent() else {
        return Vec::new();
    };
    vec![
        directory.join("codex-app-server"),
        directory.join("codex").join("bin").join("codex-app-server"),
        directory.join("codex").join("bin").join("codex"),
        directory.join("codex"),
        directory.join("libexec").join("codex-app-server"),
        directory.join("libexec").join("codex"),
    ]
}

fn is_standalone_app_server(executable: &Path) -> bool {
    executable
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "codex-app-server")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_sidecar_candidates_are_relative_to_agent() {
        let candidates = bundled_candidates(Path::new("/opt/gnomeai/gnomef-agent"));
        assert_eq!(candidates[0], Path::new("/opt/gnomeai/codex-app-server"));
        assert_eq!(
            candidates[1],
            Path::new("/opt/gnomeai/codex/bin/codex-app-server")
        );
        assert_eq!(candidates[2], Path::new("/opt/gnomeai/codex/bin/codex"));
        assert_eq!(candidates[3], Path::new("/opt/gnomeai/codex"));
    }

    #[test]
    fn distinguishes_full_cli_from_app_server_binary() {
        assert!(is_standalone_app_server(Path::new(
            "/opt/gnomeai/codex-app-server"
        )));
        assert!(!is_standalone_app_server(Path::new("/opt/gnomeai/codex")));
    }

    #[test]
    fn codex_home_is_created_private_and_writable() {
        let root = std::env::temp_dir().join(format!(
            "gnomeai-codex-home-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let path = root.join("state");
        ensure_codex_home(&path).unwrap();
        assert!(path.is_dir());
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert!(
            fs::read_dir(&path).unwrap().next().is_none(),
            "write probe must be removed"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn prompt_keeps_conversation_but_not_system_message() {
        let prompt = codex_prompt(&[
            Message::System {
                content: "secret internal system".into(),
            },
            Message::User {
                content: "change it".into(),
            },
            Message::Assistant {
                content: "working".into(),
                tool_calls: Vec::new(),
            },
        ]);
        assert!(prompt.contains("change it"));
        assert!(prompt.contains("working"));
        assert!(!prompt.contains("secret internal system"));
    }

    #[test]
    fn prompt_forwards_activated_skills_to_account_runtime() {
        let prompt = codex_prompt(&[
            Message::System {
                content: "<activated_skill name=\"rust-review\">Run clippy.</activated_skill>"
                    .into(),
            },
            Message::User {
                content: "review this".into(),
            },
        ]);
        assert!(prompt.contains("HOST CONTEXT"));
        assert!(prompt.contains("Run clippy."));
        assert!(prompt.contains("review this"));
    }

    #[test]
    fn pinned_codex_release_is_explicit() {
        assert_eq!(UPSTREAM_VERSION, "0.145.0");
        assert_eq!(UPSTREAM_COMMIT.len(), 40);
    }
}
