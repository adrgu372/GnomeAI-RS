//! Improver sessions: agentic proposer calls with an audit trail and a
//! content-addressed cache.
//!
//! Three channels:
//!
//! * `claude_cli` -- the Claude Code CLI, JSON output on stdout;
//! * `opencode` -- the OpenCode CLI (`bun run`), answer.md or JSON stream;
//! * `gnomeai` -- the in-process GnomeAI agent running one stateless session
//!   over the user's configured provider with the full tool surface, in a
//!   read-only sandbox with auto-denied approvals (the same contract upstream
//!   gets from OPENCODE_PERMISSION + throwaway XDG dirs).
//!
//! Prompts go in as the user message; empty replies are an error, never an
//! empty string.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::tree::SurfaceSpec;
use super::tree::Tree;
use super::{copy_tree, env, read_text_lossy, remove_tree_all, walk_files};
use futures_util::StreamExt;

pub const SYSTEM_ARG_MAX: usize = 120_000;

/// Environments upstream strips for the claude_cli channel; keep the same
/// hygiene for the in-process channel.
const DROP_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_BEDROCK_BASE_URL",
    "CLAUDE_CODE_USE_BEDROCK",
];

/// One session: what came back, what it cost, and where the audit trail is.
#[derive(Debug, Clone)]
pub struct Reply {
    pub label: String,
    pub text: String,
    pub ok: bool,
    pub cost_usd: f64,
    pub wall_s: f64,
    pub attempts: u32,
    pub cached: bool,
    pub channel: String,
    pub error: String,
    /// The harvested workspace holding the edited harness, when requested.
    pub workspace: Option<PathBuf>,
}

/// A call did not produce a usable reply. Never swallowed into an empty
/// string; carries the spend so a caller can accumulate it.
#[derive(Debug, thiserror::Error)]
#[error("[{label}] {kind}/{model}: {message}")]
pub struct ProposerError {
    pub label: String,
    pub kind: String,
    pub model: String,
    pub message: String,
    pub attempts: u32,
    pub cost_usd: f64,
}

impl ProposerError {
    pub fn channel(&self) -> &str {
        &self.kind
    }
}

/// Build the environment for one CLI session, upstream-style.
/// A scripted session, used by tests and by any embedding that already owns a
/// model loop (the desktop's own agent, a benchmark harness). When set, it
/// replaces the channel transports entirely.
#[async_trait::async_trait]
pub trait ScriptedSession: Send + Sync {
    async fn reply(
        &self,
        label: &str,
        workspace: &Path,
        mission: &str,
    ) -> Result<(String, f64), String>;
}

fn session_env(kind: &str, project: &str, region: &str) -> BTreeMap<String, String> {
    let mut e: BTreeMap<String, String> = std::env::vars()
        .filter(|(k, _)| !DROP_ENV.contains(&k.as_str()))
        .collect();
    if kind == "claude_cli" {
        e.insert("CLAUDE_CODE_USE_VERTEX".into(), "1".into());
        e.insert("ANTHROPIC_VERTEX_PROJECT_ID".into(), project.into());
        e.insert("CLOUD_ML_REGION".into(), region.into());
    } else {
        e.insert("GOOGLE_VERTEX_PROJECT".into(), project.into());
        e.insert("GOOGLE_VERTEX_LOCATION".into(), region.into());
        e.insert("VERTEXAI_LOCATION".into(), region.into());
    }
    e
}

/// Content-address one call, including its evidence and its workspace path.
/// The resolved path is part of the key: two modes can have byte-identical
/// k=1 evidence and must still be independent draws.
pub fn call_fingerprint(
    label: &str,
    mission: &str,
    system: &str,
    context_dir: Option<&Path>,
) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    for value in [label, mission, system] {
        h.update(value.as_bytes());
        h.update([0]);
    }
    if let Some(root) = context_dir {
        h.update(
            root.canonicalize()
                .unwrap_or_else(|_| root.to_path_buf())
                .to_string_lossy()
                .as_bytes(),
        );
        h.update([0]);
        if root.is_dir() {
            for rel in walk_files(root) {
                h.update(rel.to_string_lossy().as_bytes());
                h.update([0]);
                match std::fs::read(root.join(&rel)) {
                    Ok(bytes) => h.update(&bytes),
                    Err(_) => h.update(format!("<unreadable>").as_bytes()),
                }
                h.update([0]);
            }
        }
    }
    hex16(&h.finalize())
}

fn hex16(digest: &[u8]) -> String {
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn safe_label(label: &str) -> String {
    label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Remove harvest roots left behind by process ids that no longer exist.
///
/// A harvest root is keyed by process id and nothing deletes it, so a root from
/// an earlier run is never visited again and its workspace copy sits in the
/// temp directory forever. Roots whose owner is still alive are left alone, so
/// a concurrent run is never disturbed.
fn prune_dead_harvest_roots() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
        else {
            continue;
        };
        let Some(pid) = name
            .strip_prefix("harness2-harvest-")
            .and_then(|pid| pid.parse::<u32>().ok())
        else {
            continue;
        };
        if path.is_dir() && !Path::new(&format!("/proc/{pid}")).exists() {
            remove_tree_all(&path);
        }
    }
}

/// Runs agentic sessions on one channel, with an audit trail and a cache.
pub struct Proposer {
    pub kind: String,
    pub model: String,
    pub effort: String,
    pub steps: u32,
    pub timeout_secs: u64,
    pub attempts: u32,
    pub project: String,
    pub region: String,
    pub audit_dir: Option<PathBuf>,
    /// When set, every call goes to this session instead of a channel
    /// transport.
    pub scripted: Option<Arc<dyn ScriptedSession>>,
    /// Read-only sandbox + auto-deny for the `gnomeai` channel.
    pub auto_deny: bool,
    calls: std::sync::Mutex<Vec<Reply>>,
}

impl Proposer {
    pub fn new(kind: &str, model: &str) -> Self {
        Self {
            kind: kind.into(),
            model: model.into(),
            effort: "high".into(),
            steps: 120,
            timeout_secs: 3600,
            attempts: 2,
            project: env("GOOGLE_CLOUD_PROJECT")
                .or_else(|| env("HARNESS2_VERTEX_PROJECT"))
                .unwrap_or_default(),
            region: env_or_default("GOOGLE_CLOUD_LOCATION", "global"),
            audit_dir: None,
            auto_deny: true,
            scripted: None,
            calls: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub fn with_scripted(mut self, session: Arc<dyn ScriptedSession>) -> Self {
        self.scripted = Some(session);
        self
    }

    pub fn with_audit_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.audit_dir = Some(dir.into());
        self
    }

    pub fn recorded_calls(&self) -> Vec<Reply> {
        self.calls.lock().unwrap().clone()
    }

    /// Run one session. Errors carry the spend; a cached hit carries no cost.
    pub async fn call(
        &self,
        mission: &str,
        context_dir: Option<&Path>,
        label: &str,
        system: &str,
        max_attempts: Option<u32>,
        harvest: Option<&str>,
    ) -> Result<Reply, ProposerError> {
        if self.model.trim().is_empty() {
            return Err(ProposerError {
                label: label.into(),
                kind: self.kind.clone(),
                model: self.model.clone(),
                message: "no improver model configured; set HARNESS2_IMPROVER_MODEL or the \
                          desktop provider"
                    .into(),
                attempts: 0,
                cost_usd: 0.0,
            });
        }
        let cache_key = call_fingerprint(label, mission, system, context_dir);
        let tries = max_attempts.unwrap_or(self.attempts);
        let wall = self.timeout_secs;
        let t0 = std::time::Instant::now();
        let mut spent_usd = 0.0f64;
        let mut last = String::new();
        for attempt in 1..=tries {
            if attempt > 1 {
                tokio::time::sleep(std::time::Duration::from_secs(10 * (attempt as u64 - 1))).await;
            }
            // The cache key includes the canonical context path, so concurrent
            // runs in one process (several tasks sharing a label such as
            // `inv1`) never share or delete each other's scratch workspace.
            let tmp = std::env::temp_dir().join(format!(
                "harness2-prop-{}-{}-{}-{}",
                std::process::id(),
                safe_label(label),
                &cache_key,
                attempt
            ));
            remove_tree_all(&tmp);
            let _ = std::fs::create_dir_all(&tmp);
            if let Some(ctx) = context_dir {
                if ctx.is_dir() {
                    if let Err(exc) = copy_tree(ctx, &tmp.join("context")) {
                        last = format!("copy_tree: {exc}");
                        remove_tree_all(&tmp);
                        continue;
                    }
                }
            }
            let outcome: anyhow::Result<(String, f64, String)> =
                if let Some(scripted) = &self.scripted {
                    scripted
                        .reply(label, &tmp, mission)
                        .await
                        .map(|(text, cost)| (text, cost, String::new()))
                        .map_err(anyhow::Error::msg)
                } else if self.kind == "claude_cli" {
                    self.claude_cli(&tmp, mission, system, wall).await
                } else if self.kind == "opencode" {
                    self.opencode(&tmp, mission, system, wall).await
                } else {
                    self.gnomeai(&tmp, mission, system, wall).await
                };
            let (text, cost, err) = match outcome {
                Ok(tuple) => tuple,
                Err(exc) => {
                    spent_usd += 0.0;
                    last = format!("{exc}");
                    remove_tree_all(&tmp);
                    continue;
                }
            };
            spent_usd += cost;
            if text.trim().is_empty() {
                last = if err.is_empty() {
                    "empty reply".into()
                } else {
                    err
                };
                remove_tree_all(&tmp);
                continue;
            }
            let mut reply = Reply {
                label: label.into(),
                text,
                ok: true,
                cost_usd: spent_usd,
                wall_s: t0.elapsed().as_secs_f64(),
                attempts: attempt,
                cached: false,
                channel: self.kind.clone(),
                error: String::new(),
                workspace: None,
            };
            if let Some(sub) = harvest {
                reply.workspace = self.harvest(&tmp, sub, label, &cache_key);
                if reply.workspace.is_none() {
                    last =
                        format!("the requested harvest `{sub}` did not come back from the session");
                    remove_tree_all(&tmp);
                    continue;
                }
            }
            self.write_audit(&reply, mission, system, &cache_key);
            self.calls.lock().unwrap().push(reply.clone());
            remove_tree_all(&tmp);
            return Ok(reply);
        }
        let reply = Reply {
            label: label.into(),
            text: String::new(),
            ok: false,
            cost_usd: spent_usd,
            wall_s: t0.elapsed().as_secs_f64(),
            attempts: tries,
            cached: false,
            channel: self.kind.clone(),
            error: last.clone(),
            workspace: None,
        };
        self.write_audit(&reply, mission, system, &cache_key);
        self.calls.lock().unwrap().push(reply);
        Err(ProposerError {
            label: label.into(),
            kind: self.kind.clone(),
            model: self.model.clone(),
            message: last,
            attempts: tries,
            cost_usd: spent_usd,
        })
    }

    async fn claude_cli(
        &self,
        cwd: &Path,
        mission: &str,
        system: &str,
        wall: u64,
    ) -> anyhow::Result<(String, f64, String)> {
        let mut cmd = tokio::process::Command::new("claude");
        cmd.args([
            "--dangerously-skip-permissions",
            "-p",
            "--output-format",
            "json",
            "--effort",
        ])
        .arg(&self.effort)
        .current_dir(cwd)
        .env_clear();
        for (k, v) in session_env("claude_cli", &self.project, &self.region) {
            cmd.env(k, v);
        }
        let mut stdin_data = mission.to_string();
        if !system.is_empty() {
            if system.len() > SYSTEM_ARG_MAX {
                stdin_data = format!("# SYSTEM INSTRUCTIONS\n\n{system}\n\n# END\n\n{mission}");
                cmd.args([
                    "--append-system-prompt",
                    "Follow the SYSTEM INSTRUCTIONS block at the top of the message.",
                ]);
            } else {
                cmd.args(["--append-system-prompt", system]);
            }
        }
        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = cmd.spawn()?;
        if let Some(mut si) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            let _ = si.write_all(stdin_data.as_bytes()).await;
            si.shutdown().await.ok();
        }
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(wall),
            child.wait_with_output(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("timeout after {wall}s"))??;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let payload: serde_json::Value =
            serde_json::from_str(stdout.trim()).unwrap_or(serde_json::Value::Null);
        let mut text = payload
            .get("result")
            .or_else(|| payload.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        let mut err = String::new();
        let is_error = payload
            .get("is_error")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let subtype = payload.get("subtype").and_then(|v| v.as_str());
        if is_error || subtype.map(|s| s != "success").unwrap_or(false) {
            err = format!(
                "CLI error: {} {}",
                subtype.unwrap_or("?"),
                &text[..text.len().min(200)]
            );
            text.clear();
        } else if !out.status.success() && text.trim().is_empty() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            err = format!("rc={}: {}", out.status, &stderr[..stderr.len().min(300)]);
        }
        let cost = payload
            .get("total_cost_usd")
            .and_then(|c| c.as_f64())
            .unwrap_or(0.0);
        Ok((text, cost, err))
    }

    async fn opencode(
        &self,
        cwd: &Path,
        mission: &str,
        system: &str,
        wall: u64,
    ) -> anyhow::Result<(String, f64, String)> {
        std::fs::write(
            cwd.join("MISSION.md"),
            if system.is_empty() {
                mission.to_string()
            } else {
                format!("{system}\n\n---\n\n{mission}")
            },
        )?;
        std::fs::create_dir_all(cwd.join("output"))?;
        std::fs::write(
            cwd.join("opencode.json"),
            serde_json::json!({"agent": {"build": {"steps": self.steps}}}).to_string(),
        )?;
        let prompt = "Read MISSION.md first. Do what it asks. Reply with the requested output \
                      as your final message AND save it verbatim to output/answer.md.";
        let oc = opencode_dir();
        let bun = config_bun().unwrap_or_else(|| oc.join("packages/opencode"));
        let mut cmd = tokio::process::Command::new(&bun);
        cmd.args([
            "run",
            "--cwd",
            &oc.join("packages/opencode").to_string_lossy(),
            "--conditions=browser",
            "src/index.ts",
            "run",
            prompt,
            "--dir",
            &cwd.to_string_lossy(),
            "--model",
            &self.model,
            "--format",
            "json",
        ]);
        if !self.effort.is_empty() {
            cmd.args(["--variant", &self.effort]);
        }
        cmd.current_dir(cwd).env_clear();
        for (k, v) in session_env("opencode", &self.project, &self.region) {
            cmd.env(k, v);
        }
        cmd.env(
            "OPENCODE_PERMISSION",
            serde_json::json!({
                "external_directory": {"*": "deny", cwd.to_string_lossy().to_string(): "allow", format!("{}/**", cwd.to_string_lossy()): "allow"}
            })
            .to_string(),
        );
        cmd.env("XDG_DATA_HOME", cwd.join(".ocdata"));
        cmd.env("XDG_STATE_HOME", cwd.join(".ocstate"));
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let child = cmd.spawn()?;
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(wall),
            child.wait_with_output(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("timeout after {wall}s"))??;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let cost = opencode_cost(&stdout);
        let ans = cwd.join("output/answer.md");
        if let Ok(body) = read_text_lossy(&ans) {
            if !body.trim().is_empty() {
                return Ok((body, cost, String::new()));
            }
        }
        let (text, err) = opencode_stdout(&stdout);
        if !text.trim().is_empty() {
            return Ok((text, cost, String::new()));
        }
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        let tail: String = stderr
            .lines()
            .filter(|l| !l.contains("OPENCODE") && !l.contains("is not a recognized"))
            .collect::<Vec<_>>()
            .join("\n");
        let tail = tail
            .chars()
            .rev()
            .take(300)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>();
        Ok((
            String::new(),
            cost,
            format!(
                "rc={}: {}",
                out.status,
                if err.is_empty() { tail } else { err }
            ),
        ))
    }

    /// The in-process GnomeAI channel: one stateless session over the
    /// user's configured provider with the full tool surface, in a read-only
    /// sandbox, approvals auto-denied. Evidence is embedded in the mission (the
    /// file pointer block) and the reply is the final message; a temporary
    /// Store row backs the session so the transcript stays inspectable.
    async fn gnomeai(
        &self,
        cwd: &Path,
        mission: &str,
        system: &str,
        _wall: u64,
    ) -> anyhow::Result<(String, f64, String)> {
        let app_home = gnomeai_app_home()?;
        let cfg = crate::config::AppConfig::load(&app_home.join("config.json"))?;
        let selection =
            crate::provider_catalog::ProviderSelection::from_choice(&self.model, None, None);
        let selection = match selection {
            Ok(sel) => sel,
            Err(_) => {
                // Accept bare OpenAI-compatible endpoints as `custom`.
                crate::provider_catalog::ProviderSelection::legacy(
                    cfg.llama_base_url.trim_end_matches('/').to_string(),
                    (!cfg.llama_api_key.trim().is_empty())
                        .then(|| cfg.llama_api_key.trim().to_string()),
                    self.model.clone(),
                )
            }
        };
        let model = selection.model.clone();
        let provider_id = selection.provider_id.clone();
        let provider = crate::provider_catalog::build_provider(
            &selection,
            cwd,
            crate::sandbox::SandboxMode::ReadOnly,
        )?;
        let workspace = cwd.to_path_buf();
        let policy = crate::sandbox::SandboxPolicy::read_only(&workspace);
        let registry = proposer_registry(&workspace, &app_home, &cfg, &policy)?;
        let store = crate::store::Store::open(&app_home.join("store/harness2-improver.db"))?;
        let session = store.create_session(&workspace, &model)?;
        let (event_tx, _event_rx) = tokio::sync::mpsc::channel(256);
        let (approval_tx, approval_rx) = tokio::sync::mpsc::channel(64);
        let agent = crate::agent::Agent::new(
            provider,
            Arc::new(registry),
            store,
            session.id.clone(),
            model.clone(),
            provider_id,
            self.effort.clone(),
            crate::agent::ApprovalPolicy::Ask,
            workspace,
            policy,
            Arc::new(crate::tooling::ToolOutputStore::new(
                app_home.join("store/harness2-tool_outputs"),
            )?),
            Vec::new(),
            event_tx.clone(),
            approval_rx,
        );
        // Drain events and refuse every approval: the session is autonomous.
        let drainer = tokio::spawn(async move {
            let mut events = _event_rx;
            while let Some(event) = events.recv().await {
                let _ = event; // transcript events are not consumed here
            }
        });
        let approver = tokio::spawn(async move {
            let mut approvals = approval_tx;
            loop {
                // Approvals arrive on Agent's receiver; we hold the sender to
                // keep the channel alive. The agent times out into denial.
                if approvals.is_closed() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        });
        let cancel = tokio_util::sync::CancellationToken::new();
        let run = agent.run_turn(mission.to_string(), cancel.clone());
        let result = tokio::time::timeout(std::time::Duration::from_secs(_wall), run).await;
        drainer.abort();
        approver.abort();
        cancel.cancel();
        match result {
            Err(_) => Err(anyhow::anyhow!("gnomeai session timed out")),
            Ok(Err(exc)) => Err(anyhow::anyhow!("gnomeai session failed: {exc}")),
            Ok(Ok(())) => {
                // The final assistant turn is the reply.
                let store =
                    crate::store::Store::open(&app_home.join("store/harness2-improver.db"))?;
                let turns = store.live_turns(&session.id)?;
                let text = turns
                    .iter()
                    .rev()
                    .find(|t| t.role == "assistant" && !t.content.trim().is_empty())
                    .map(|t| t.content.clone())
                    .unwrap_or_default();
                if text.trim().is_empty() {
                    return Ok((
                        String::new(),
                        0.0,
                        "empty reply from in-process session".into(),
                    ));
                }
                Ok((text, 0.0, String::new()))
            }
        }
    }

    /// Copy `tmp/<sub>` somewhere durable before the session directory goes.
    fn harvest(&self, tmp: &Path, sub: &str, label: &str, cache_key: &str) -> Option<PathBuf> {
        prune_dead_harvest_roots();
        let src = tmp.join(sub);
        if !src.is_dir() {
            return None;
        }
        let dest_root = self.audit_dir.clone().unwrap_or_else(|| {
            std::env::temp_dir().join(format!("harness2-harvest-{}", std::process::id()))
        });
        let _ = std::fs::create_dir_all(&dest_root);
        let mut safe = safe_label(label);
        if !cache_key.is_empty() {
            safe = format!("{safe}.{cache_key}");
        }
        let dest = dest_root.join(format!("{safe}.workspace"));
        remove_tree_all(&dest);
        copy_tree(&src, &dest).ok()?;
        Some(dest)
    }

    fn write_audit(&self, reply: &Reply, mission: &str, system: &str, cache_key: &str) {
        let Some(dir) = &self.audit_dir else {
            return;
        };
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
        let record = serde_json::json!({
            "label": reply.label,
            "ok": reply.ok,
            "channel": reply.channel,
            "model": self.model,
            "cost_usd": reply.cost_usd,
            "wall_s": reply.wall_s,
            "attempts": reply.attempts,
            "cached": reply.cached,
            "error": reply.error,
            "cache_key": cache_key,
            "text_preview": reply.text.chars().take(2000).collect::<String>(),
            "mission_chars": mission.len(),
            "system_chars": system.len(),
        });
        let _ = std::fs::write(
            dir.join(format!("{}.json", safe_label(&reply.label))),
            serde_json::to_vec_pretty(&record).unwrap_or_default(),
        );
    }
}

fn env_or_default(name: &str, default: &str) -> String {
    env(name).unwrap_or_else(|| default.into())
}

fn opencode_dir() -> PathBuf {
    env("HARNESS2_OPENCODE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("dependencies/opencode"))
}

fn config_bun() -> Option<PathBuf> {
    let p = PathBuf::from("dependencies/tools/node_modules/.bin/bun");
    p.is_file().then_some(p)
}

fn gnomeai_config_path() -> PathBuf {
    super::runs_root()
        .ancestors()
        .nth(2)
        .map(|p| p.join("config.json"))
        .unwrap_or_else(|| PathBuf::from("config.json"))
}

/// The app home the in-process sessions use. Under `GNOMEF_RS_HOME` when set,
/// else the user's state dir, so sweeps reuse the configured provider keys.
fn gnomeai_app_home() -> anyhow::Result<PathBuf> {
    let state = match env("GNOMEF_RS_HOME") {
        Some(home) => PathBuf::from(home),
        None => {
            let base = std::env::var("XDG_STATE_HOME").unwrap_or_else(|_| {
                format!(
                    "{}/.local/state",
                    std::env::var("HOME").unwrap_or_else(|_| ".".into())
                )
            });
            PathBuf::from(base).join("gnomeai-rs")
        }
    };
    std::fs::create_dir_all(state.join("store"))?;
    Ok(state)
}

/// The tool surface an in-proc improver session gets: inspection, search,
/// patch (refused by the read-only policy) and shell. No desktop, node or
/// privileged tools: the session is autonomous and unattended.
fn proposer_registry(
    workspace: &Path,
    app_home: &Path,
    cfg: &crate::config::AppConfig,
    policy: &crate::sandbox::SandboxPolicy,
) -> anyhow::Result<crate::tooling::Registry> {
    let mut registry = crate::tooling::Registry::default();
    let paths = crate::storage::AppPaths::new(app_home.to_path_buf())?;
    let (event_tx, _event_rx) = tokio::sync::mpsc::channel(8);
    let broker = Arc::new(crate::privilege::PrivilegeBroker::new(event_tx, {
        // Privilege replies have no listener in an unattended session; the
        // broker sees a closed receiver and fails closed.
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        drop(tx);
        rx
    }));
    let _ = _event_rx;
    let config = Arc::new(tokio::sync::RwLock::new(cfg.clone()));
    let output_store = Arc::new(crate::tooling::ToolOutputStore::new(
        app_home.join("store/harness2-tool_outputs"),
    )?);
    crate::tools::register_all(
        &mut registry,
        workspace,
        &paths.generated_dir,
        policy.clone(),
        config,
        output_store,
        broker,
        String::new(),
        String::new(),
    );
    Ok(registry)
}

fn opencode_cost(stdout: &str) -> f64 {
    let mut total = 0.0f64;
    for line in stdout.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(ev) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let part = ev.get("part").unwrap_or(&ev);
        let typ = part
            .get("type")
            .or_else(|| ev.get("type"))
            .and_then(|t| t.as_str())
            .unwrap_or("");
        if typ == "step-finish" || typ == "step_finish" {
            if let Some(c) = part.get("cost").and_then(|c| c.as_f64()) {
                total += c;
            }
        }
    }
    super::ledger::round6(total)
}

fn opencode_stdout(stdout: &str) -> (String, String) {
    let mut texts = Vec::new();
    let mut errors = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(ev) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let part = ev.get("part").cloned().unwrap_or_else(|| ev.clone());
        let typ = part
            .get("type")
            .or_else(|| ev.get("type"))
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();
        if typ == "text" {
            if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                texts.push(t.to_string());
            }
        } else if typ == "error" {
            if let Some(m) = part.get("message").or(Some(&part)).and_then(|v| v.as_str()) {
                errors.push(m.to_string());
            }
        }
    }
    if texts.is_empty() {
        (
            String::new(),
            errors.join("; ")[..300.min(errors.join("; ").len())].to_string(),
        )
    } else {
        (
            texts.join("\n\n"),
            errors.join("; ")[..300.min(errors.join("; ").len())].to_string(),
        )
    }
}

/// The tree surface a proposer of this channel edits; used by recursion to
/// pick the harvest root.
pub fn harvest_root(surface: &SurfaceSpec) -> &'static str {
    let _ = surface;
    "context/harness"
}

/// Convenience: the working-tree form of a harvested workspace, as a Tree.
pub fn workspace_tree(surface: &SurfaceSpec, workspace: &Path) -> (Tree, Vec<String>) {
    let mut rejected = Vec::new();
    let tree = super::tree::read_tree_with_rejects(surface, workspace, Some(&mut rejected));
    (tree, rejected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_covers_mission_and_context() {
        let a = call_fingerprint("l", "m", "", None);
        let b = call_fingerprint("l", "m2", "", None);
        assert_ne!(a, b);
        let tmp = std::env::temp_dir().join(format!("h2-fp-{}", std::process::id()));
        remove_tree_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("evidence.md"), "one").unwrap();
        let c = call_fingerprint("l", "m", "", Some(&tmp));
        std::fs::write(tmp.join("evidence.md"), "two").unwrap();
        let d = call_fingerprint("l", "m", "", Some(&tmp));
        assert_ne!(c, d);
        remove_tree_all(&tmp);
    }

    #[tokio::test]
    async fn empty_model_is_rejected_before_any_call() {
        let p = Proposer::new("gnomeai", "");
        let err = p.call("m", None, "lbl", "", None, None).await;
        assert!(err.is_err());
        assert_eq!(err.unwrap_err().attempts, 0);
    }

    #[test]
    fn opencode_cost_sums_step_finish() {
        let stream = "{\"part\":{\"type\":\"step-finish\",\"cost\":0.5}}\nnot json\n{\"part\":{\"type\":\"step-finish\",\"cost\":0.25}}\n";
        assert!((opencode_cost(stream) - 0.75).abs() < 1e-9);
    }
}
