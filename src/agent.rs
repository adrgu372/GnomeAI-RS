//! The coding-agent loop — the spine everything else hangs off.
//!
//! One turn is: call the model, stream it out, dispatch whatever tools it
//! asked for, persist, verify if anything changed on disk, repeat until the
//! model stops asking for tools or a budget runs out.
//!
//! Four things that are easy to get wrong and expensive to fix later:
//!
//!   1. Interrupt has to land mid-stream. Checking a flag between turns means
//!      Ctrl+C does nothing during the ninety seconds of `cargo build`, which
//!      is the only time anyone presses it.
//!
//!   2. Tool results are persisted before the next model call, always. Crash
//!      in between and you resume a session whose last assistant message asks
//!      for a tool that has no result — every provider rejects that, and the
//!      error never mentions it.
//!
//!   3. Read-only tools run concurrently, writing tools run one at a time. Two
//!      `apply_patch` calls racing on the same file is a corrupted file.
//!
//!   4. Compaction is checked *before* building the request, not after the
//!      provider rejects it for being too long.

use anyhow::Result;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

use crate::protocol::{Decision, Event};
use crate::provider::{Delta, Message, Provider, Request, StopReason, ToolCall, ToolSpec};
use crate::sandbox::SandboxPolicy;
use crate::store::Store;
use crate::verify;

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

pub struct FilePatch {
    /// Workspace-relative path.
    pub path: PathBuf,
    pub before: Option<String>,
    pub after: Option<String>,
    pub diff: String,
}

pub struct ToolOutcome {
    /// What the model sees.
    pub content: String,
    pub ok: bool,
    /// Files this call wrote, if any. Non-empty triggers verification.
    pub touched: Vec<PathBuf>,
    /// Reversible file changes, normally produced by `apply_patch`.
    pub patches: Vec<FilePatch>,
}

#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    /// False for anything that writes or executes. Drives both the concurrency
    /// decision and the default approval policy.
    fn read_only(&self) -> bool {
        false
    }
    async fn call(&self, args: Value, cancel: &CancellationToken) -> Result<ToolOutcome>;
}

#[derive(Default)]
pub struct Registry {
    tools: Vec<Arc<dyn Tool>>,
}

impl Registry {
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.push(tool);
    }
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|t| t.spec()).collect()
    }
    fn find(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.spec().name == name).cloned()
    }
}

// ---------------------------------------------------------------------------
// Approval
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalPolicy {
    /// Ask for every tool that writes or executes.
    Ask,
    /// Run tools without asking. Used only by explicit `full-access`.
    Never,
}

// ---------------------------------------------------------------------------
// Agent
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct Agent {
    pub provider: Arc<dyn Provider>,
    pub registry: Arc<Registry>,
    pub store: Store,
    pub session_id: String,
    pub model: String,
    pub approval: ApprovalPolicy,
    pub max_rounds: usize,
    pub context_budget: i64,
    pub workspace: PathBuf,
    pub verify_policy: SandboxPolicy,

    events: mpsc::Sender<Event>,
    /// Approvals arrive out of band from whichever interface is attached.
    approvals: Arc<Mutex<mpsc::Receiver<(String, Decision)>>>,
    always_allow: Arc<Mutex<Vec<String>>>,
}

impl Agent {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider: Arc<dyn Provider>,
        registry: Arc<Registry>,
        store: Store,
        session_id: String,
        model: String,
        approval: ApprovalPolicy,
        max_rounds: usize,
        context_budget: i64,
        workspace: PathBuf,
        verify_policy: SandboxPolicy,
        events: mpsc::Sender<Event>,
        approvals: mpsc::Receiver<(String, Decision)>,
    ) -> Self {
        Self {
            provider,
            registry,
            store,
            session_id,
            model,
            approval,
            max_rounds,
            context_budget,
            workspace,
            verify_policy,
            events,
            approvals: Arc::new(Mutex::new(approvals)),
            always_allow: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn event_sender(&self) -> mpsc::Sender<Event> {
        self.events.clone()
    }

    pub async fn switch_session(&mut self, session_id: String) {
        self.session_id = session_id;
        self.always_allow.lock().await.clear();
    }

    pub async fn run_turn(&self, user_text: String, cancel: CancellationToken) -> Result<()> {
        let started = Instant::now();
        let first_user_turn = !self
            .store
            .live_turns(&self.session_id)?
            .iter()
            .any(|turn| turn.role == "user" && !turn.is_summary);
        let turn = self.store.append_turn(
            &self.session_id,
            "user",
            &user_text,
            estimate(&user_text),
            first_user_turn,
        )?;

        let _ = self
            .events
            .send(Event::TurnStarted { turn_id: turn.id })
            .await;

        if self.provider.delegates_execution()
            && self.approval != ApprovalPolicy::Never
            && !self.approve_delegated_turn().await?
        {
            let text = "Delegated account-provider execution was denied by the user.";
            self.store.append_turn(
                &self.session_id,
                "assistant",
                &serde_json::to_string(&Message::Assistant {
                    content: text.into(),
                    tool_calls: Vec::new(),
                })?,
                estimate(text),
                false,
            )?;
            let _ = self.events.send(Event::Token { text: text.into() }).await;
            let _ = self
                .events
                .send(Event::TurnCompleted {
                    turn_id: turn.id,
                    input_tokens: 0,
                    output_tokens: 0,
                    duration_ms: started.elapsed().as_millis() as u64,
                })
                .await;
            return Ok(());
        }

        let mut round = 0usize;
        let mut usage_in = 0i64;
        let mut usage_out = 0i64;

        loop {
            round += 1;
            if round > self.max_rounds {
                self.emit_error("round budget exhausted", false).await;
                break;
            }
            if cancel.is_cancelled() {
                let _ = self.events.send(Event::Interrupted).await;
                break;
            }

            self.compact_if_needed().await?;

            let messages = self.build_messages()?;
            let req = Request {
                model: self.model.clone(),
                messages,
                tools: self.registry.specs(),
                max_tokens: 8192,
            };

            // ---- stream ----
            let mut stream = self.provider.stream(req).await?;
            let mut text = String::new();
            let mut calls: Vec<ToolCall> = Vec::new();
            let mut stop = StopReason::Stop;
            let mut round_output_tokens = 0i64;

            loop {
                let next = tokio::select! {
                    biased;
                    // Cancellation wins the race deliberately: an interrupt
                    // that waits for the next token is not an interrupt.
                    _ = cancel.cancelled() => {
                        stop = StopReason::Cancelled;
                        None
                    }
                    d = futures_util::StreamExt::next(&mut stream) => d,
                };

                let Some(delta) = next else { break };

                match delta? {
                    Delta::Text(t) => {
                        text.push_str(&t);
                        let _ = self.events.send(Event::Token { text: t }).await;
                    }
                    Delta::Reasoning(t) => {
                        let _ = self.events.send(Event::Reasoning { text: t }).await;
                    }
                    Delta::ToolCall(c) => calls.push(c),
                    Delta::Done { reason, usage } => {
                        stop = reason;
                        usage_in += usage.input_tokens;
                        usage_out += usage.output_tokens;
                        round_output_tokens = usage.output_tokens;
                    }
                }
            }

            // Persist before anything else can fail.
            let assistant = self.store.append_turn(
                &self.session_id,
                "assistant",
                &serde_json::to_string(&Message::Assistant {
                    content: text.clone(),
                    tool_calls: calls.clone(),
                })?,
                estimate(&text),
                false,
            )?;
            if round_output_tokens > 0 {
                self.store.set_tokens(assistant.id, round_output_tokens)?;
            }

            if stop == StopReason::Cancelled {
                let _ = self.events.send(Event::Interrupted).await;
                break;
            }
            if calls.is_empty() {
                break;
            }

            // ---- dispatch ----
            let touched = self.dispatch(&calls, assistant.id, &cancel).await?;

            // ---- verify ----
            if !touched.is_empty() {
                self.verify_and_report(&cancel).await?;
            }
        }

        let _ = self
            .events
            .send(Event::TurnCompleted {
                turn_id: turn.id,
                input_tokens: usage_in,
                output_tokens: usage_out,
                duration_ms: started.elapsed().as_millis() as u64,
            })
            .await;

        Ok(())
    }

    // -----------------------------------------------------------------------

    async fn dispatch(
        &self,
        calls: &[ToolCall],
        turn_id: i64,
        cancel: &CancellationToken,
    ) -> Result<Vec<PathBuf>> {
        let mut touched = Vec::new();

        // Split by side effects. Reads fan out; writes stay in the order the
        // model asked for, because it is often editing the same file twice.
        let (reads, writes): (Vec<_>, Vec<_>) = calls.iter().cloned().partition(|c| {
            self.registry
                .find(&c.name)
                .map(|t| t.read_only())
                .unwrap_or(false)
        });

        let mut handles = Vec::new();
        for call in reads {
            let this = self.clone_handle();
            let cancel = cancel.clone();
            handles.push(tokio::spawn(async move {
                this.run_one(call, turn_id, &cancel).await
            }));
        }
        for h in handles {
            if let Ok(Ok(mut t)) = h.await {
                touched.append(&mut t);
            }
        }

        for call in writes {
            if cancel.is_cancelled() {
                break;
            }
            touched.append(&mut self.run_one(call, turn_id, cancel).await?);
        }

        Ok(touched)
    }

    async fn run_one(
        &self,
        call: ToolCall,
        turn_id: i64,
        cancel: &CancellationToken,
    ) -> Result<Vec<PathBuf>> {
        let Some(tool) = self.registry.find(&call.name) else {
            self.record_result(
                &call,
                turn_id,
                &format!("unknown tool `{}`", call.name),
                false,
            )
            .await?;
            return Ok(Vec::new());
        };

        let summary = summarise(&call);
        let _ = self
            .events
            .send(Event::ToolCallStarted {
                call_id: call.id.clone(),
                name: call.name.clone(),
                summary: summary.clone(),
            })
            .await;

        if !self.approved(&call, &tool, &summary).await? {
            self.record_result(&call, turn_id, "denied by the user", false)
                .await?;
            return Ok(Vec::new());
        }

        // A malformed argument object is feedback for the model, not a crash.
        let args: Value = match serde_json::from_str(&call.arguments) {
            Ok(v) => v,
            Err(e) => {
                self.record_result(&call, turn_id, &format!("invalid arguments: {e}"), false)
                    .await?;
                return Ok(Vec::new());
            }
        };

        let started = Instant::now();
        let outcome = match tool.call(args, cancel).await {
            Ok(o) => o,
            Err(e) => ToolOutcome {
                content: format!("tool failed: {e}"),
                ok: false,
                touched: Vec::new(),
                patches: Vec::new(),
            },
        };

        for patch in &outcome.patches {
            self.store.record_patch(
                &self.session_id,
                Some(turn_id),
                &patch.path,
                patch.before.as_deref(),
                patch.after.as_deref(),
                &patch.diff,
            )?;
        }
        if !outcome.patches.is_empty() {
            let _ = self
                .events
                .send(Event::PatchApplied {
                    files: outcome
                        .patches
                        .iter()
                        .map(|patch| self.workspace.join(&patch.path))
                        .collect(),
                    diff: outcome
                        .patches
                        .first()
                        .map(|patch| patch.diff.clone())
                        .unwrap_or_default(),
                })
                .await;
        }

        let _ = self
            .events
            .send(Event::ToolCallEnded {
                call_id: call.id.clone(),
                ok: outcome.ok,
                duration_ms: started.elapsed().as_millis() as u64,
            })
            .await;

        self.record_result(&call, turn_id, &outcome.content, outcome.ok)
            .await?;
        Ok(outcome.touched)
    }

    async fn approved(&self, call: &ToolCall, tool: &Arc<dyn Tool>, summary: &str) -> Result<bool> {
        if tool.read_only() || self.approval == ApprovalPolicy::Never {
            return Ok(true);
        }
        if self.always_allow.lock().await.iter().any(|s| s == summary) {
            return Ok(true);
        }

        self.request_approval(
            call.id.clone(),
            summary.to_string(),
            format!("`{}` writes or executes with normal OS access", call.name),
        )
        .await
    }

    async fn approve_delegated_turn(&self) -> Result<bool> {
        let summary = format!("delegate this turn to {}", self.provider.name());
        if self.always_allow.lock().await.iter().any(|s| s == &summary) {
            return Ok(true);
        }
        self.request_approval(
            format!("provider-turn-{}", uuid::Uuid::new_v4().simple()),
            summary,
            "the account-backed coding provider executes its own commands with full OS access"
                .into(),
        )
        .await
    }

    async fn request_approval(
        &self,
        call_id: String,
        command: String,
        reason: String,
    ) -> Result<bool> {
        let _ = self
            .events
            .send(Event::ApprovalRequest {
                call_id: call_id.clone(),
                command: command.clone(),
                cwd: self.workspace.clone(),
                reason,
            })
            .await;

        // Blocks the loop, which is the point — nothing else should proceed.
        let mut rx = self.approvals.lock().await;
        while let Some((id, decision)) = rx.recv().await {
            if id != call_id {
                continue;
            }
            return Ok(match decision {
                Decision::Allow => true,
                Decision::AlwaysAllow => {
                    self.always_allow.lock().await.push(command);
                    true
                }
                Decision::Deny => false,
            });
        }
        Ok(false)
    }

    async fn record_result(
        &self,
        call: &ToolCall,
        turn_id: i64,
        content: &str,
        _ok: bool,
    ) -> Result<()> {
        let msg = Message::Tool {
            call_id: call.id.clone(),
            content: content.to_string(),
        };
        self.store.append_turn(
            &self.session_id,
            "tool",
            &serde_json::to_string(&msg)?,
            estimate(content),
            false,
        )?;
        let _ = turn_id;
        Ok(())
    }

    async fn verify_and_report(&self, cancel: &CancellationToken) -> Result<()> {
        if cancel.is_cancelled() {
            return Ok(());
        }

        let report =
            verify::verify_with_cancel(&self.workspace, &self.verify_policy, cancel).await?;
        if cancel.is_cancelled() {
            return Ok(());
        }
        let summary = verify::render(&report);
        let stage = if report.toolchain_missing {
            "skipped".to_string()
        } else {
            report
                .failed_stage
                .map(|stage| stage.label().to_string())
                .unwrap_or_else(|| "all".to_string())
        };
        let passed = report.is_green();

        let _ = self
            .events
            .send(Event::Verification {
                stage,
                passed,
                summary: summary.clone(),
            })
            .await;

        // Verification is a user-role turn so the model treats it as external
        // ground truth rather than one of its own claims.
        self.store.append_turn(
            &self.session_id,
            "user",
            &format!("[automatic verification]\n{summary}"),
            estimate(&summary),
            false,
        )?;
        Ok(())
    }

    async fn compact_if_needed(&self) -> Result<()> {
        let Some(plan) = self
            .store
            .plan_compaction(&self.session_id, self.context_budget, 6)?
        else {
            return Ok(());
        };
        self.commit_compaction(plan).await
    }

    pub async fn force_compact(&self) -> Result<bool> {
        let Some(plan) = self.store.plan_compaction(&self.session_id, 0, 2)? else {
            return Ok(false);
        };
        self.commit_compaction(plan).await?;
        Ok(true)
    }

    async fn commit_compaction(&self, plan: crate::store::CompactionPlan) -> Result<()> {
        let mut source = String::new();
        for turn in &plan.victims {
            let content: String = turn.content.chars().take(4_000).collect();
            source.push_str(&format!("{}: {}\n\n", turn.role, content));
            if source.len() >= 32_000 {
                source = source.chars().take(32_000).collect();
                source.push_str("\n[older content truncated]");
                break;
            }
        }

        let request = Request {
            model: self.model.clone(),
            messages: vec![
                Message::System {
                    content: "Summarize the conversation state for another coding-agent turn. \
                              Preserve the user's goal, decisions, files changed, tool results, \
                              errors, and unfinished work. Be concise and factual."
                        .into(),
                },
                Message::User {
                    content: source.clone(),
                },
            ],
            tools: Vec::new(),
            max_tokens: 2_048,
        };

        let mut summary = String::new();
        if let Ok(mut stream) = self.provider.stream(request).await {
            while let Some(delta) = futures_util::StreamExt::next(&mut stream).await {
                match delta {
                    Ok(Delta::Text(text)) => summary.push_str(&text),
                    Ok(Delta::Done { .. }) => break,
                    Ok(_) => {}
                    Err(_) => {
                        summary.clear();
                        break;
                    }
                }
            }
        }
        if summary.trim().is_empty() {
            summary = format!(
                "Compacted conversation (provider summary unavailable):\n{}",
                source.chars().take(12_000).collect::<String>()
            );
        }

        self.store
            .commit_compaction(&self.session_id, &plan, &summary, estimate(&summary))?;

        let _ = self
            .events
            .send(Event::Compacted {
                freed_tokens: plan.freed_tokens,
            })
            .await;
        Ok(())
    }

    fn build_messages(&self) -> Result<Vec<Message>> {
        let turns = self.store.live_turns(&self.session_id)?;
        let mut out = Vec::with_capacity(turns.len());
        for t in turns {
            out.push(match t.role.as_str() {
                "system" => Message::System { content: t.content },
                "user" => Message::User { content: t.content },
                _ => serde_json::from_str(&t.content)?,
            });
        }
        Ok(out)
    }

    async fn emit_error(&self, message: &str, fatal: bool) {
        let _ = self
            .events
            .send(Event::Error {
                message: message.to_string(),
                fatal,
            })
            .await;
    }

    fn clone_handle(&self) -> Self {
        Self {
            provider: self.provider.clone(),
            registry: self.registry.clone(),
            store: self.store.clone(),
            session_id: self.session_id.clone(),
            model: self.model.clone(),
            approval: self.approval,
            max_rounds: self.max_rounds,
            context_budget: self.context_budget,
            workspace: self.workspace.clone(),
            verify_policy: self.verify_policy.clone(),
            events: self.events.clone(),
            approvals: self.approvals.clone(),
            always_allow: self.always_allow.clone(),
        }
    }
}

/// Good enough. Correct it from the provider's `usage` once the turn lands —
/// wiring a real tokeniser is wasted effort when every local model ships a
/// different one.
fn estimate(s: &str) -> i64 {
    (s.len() / 4) as i64
}

/// One line the user can judge without reading JSON. Approval prompts are only
/// as useful as this string.
fn summarise(call: &ToolCall) -> String {
    let v: Value = serde_json::from_str(&call.arguments).unwrap_or(Value::Null);
    match call.name.as_str() {
        "shell" => v["command"].as_str().unwrap_or("?").to_string(),
        "apply_patch" => {
            let n = v["patch"].as_str().unwrap_or("").matches("*** ").count();
            format!("patch touching {n} file(s)")
        }
        "read_file" => format!("read {}", v["path"].as_str().unwrap_or("?")),
        "search" => format!("search {:?}", v["pattern"].as_str().unwrap_or("?")),
        "activate_skill" => format!("activate skill {}", v["name"].as_str().unwrap_or("?")),
        "read_skill_resource" => format!(
            "read skill resource {}/{}",
            v["skill"].as_str().unwrap_or("?"),
            v["path"].as_str().unwrap_or("?")
        ),
        other => other.to_string(),
    }
}
