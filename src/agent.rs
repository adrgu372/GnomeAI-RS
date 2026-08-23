//! The coding-agent loop — the spine everything else hangs off.
//!
//! One turn is: call the model, stream it out, dispatch whatever tools it
//! asked for, persist, verify if anything changed on disk, repeat until the
//! model stops asking for tools or the user interrupts.
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
use crate::provider::{Delta, Message, Provider, Request, StopReason, ToolCall};
use crate::sandbox::SandboxPolicy;
use crate::store::Store;
use crate::tooling::{
    ApprovalRequirement, Registry, Tool, ToolConcurrency, ToolOutcome, ToolOutputStore,
};
use crate::verify;

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
    pub context_budget: i64,
    pub workspace: PathBuf,
    pub verify_policy: SandboxPolicy,
    pub output_store: Arc<ToolOutputStore>,

    events: mpsc::Sender<Event>,
    /// Approvals arrive out of band from whichever interface is attached.
    approvals: Arc<Mutex<mpsc::Receiver<(String, Decision)>>>,
    always_allow: Arc<Mutex<Vec<String>>>,
}

#[derive(Default)]
struct CompletedToolCall {
    touched: Vec<PathBuf>,
    desktop_screenshot: Option<PathBuf>,
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
        context_budget: i64,
        workspace: PathBuf,
        verify_policy: SandboxPolicy,
        output_store: Arc<ToolOutputStore>,
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
            context_budget,
            workspace,
            verify_policy,
            output_store,
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
        let visible_user_text = crate::provider::user_content_for_display(&user_text);
        let first_user_turn = !self
            .store
            .live_turns(&self.session_id)?
            .iter()
            .any(|turn| turn.role == "user" && !turn.is_summary);
        let turn = self.store.append_turn(
            &self.session_id,
            "user",
            &user_text,
            estimate(&visible_user_text),
            first_user_turn,
        )?;
        if first_user_turn {
            self.store.rename_session(
                &self.session_id,
                &automatic_session_title(&visible_user_text),
            )?;
        }

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

        let mut usage_in = 0i64;
        let mut usage_out = 0i64;

        loop {
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
        let mut desktop_screenshots = Vec::new();

        // Split by side effects. Reads fan out; writes stay in the order the
        // model asked for, because it is often editing the same file twice.
        let (reads, writes): (Vec<_>, Vec<_>) = calls.iter().cloned().partition(|c| {
            self.registry
                .find(&c.name)
                .map(|tool| tool.definition().concurrency == ToolConcurrency::Parallel)
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
            if let Ok(Ok(mut completed)) = h.await {
                touched.append(&mut completed.touched);
                if let Some(path) = completed.desktop_screenshot {
                    desktop_screenshots.push(path);
                }
            }
        }

        for call in writes {
            if cancel.is_cancelled() {
                break;
            }
            let mut completed = self.run_one(call, turn_id, cancel).await?;
            touched.append(&mut completed.touched);
            if let Some(path) = completed.desktop_screenshot {
                desktop_screenshots.push(path);
            }
        }

        // Every tool result must immediately follow the assistant's tool-call
        // message. Attach visual feedback only after the whole batch has been
        // recorded, otherwise a screenshot user turn could split parallel tool
        // results and make the next provider request invalid.
        for path in desktop_screenshots {
            self.record_desktop_screenshot(&path)?;
        }

        Ok(touched)
    }

    async fn run_one(
        &self,
        call: ToolCall,
        turn_id: i64,
        cancel: &CancellationToken,
    ) -> Result<CompletedToolCall> {
        let Some(tool) = self.registry.find(&call.name) else {
            self.record_result(
                &call,
                turn_id,
                &format!("unknown tool `{}`", call.name),
                false,
            )
            .await?;
            return Ok(CompletedToolCall::default());
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

        // A malformed argument object is feedback for the model, not a crash.
        let args: Value = match serde_json::from_str(&call.arguments) {
            Ok(v) => v,
            Err(e) => {
                self.record_result(&call, turn_id, &format!("invalid arguments: {e}"), false)
                    .await?;
                return Ok(CompletedToolCall::default());
            }
        };

        if !self.approved(&call, &tool, &summary).await? {
            self.record_result(&call, turn_id, "denied by the user", false)
                .await?;
            return Ok(CompletedToolCall::default());
        }

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

        let presented = self
            .output_store
            .present(&outcome.content)
            .unwrap_or_else(|error| {
                let preview: String = outcome.content.chars().take(32 * 1024).collect();
                format!("{preview}\n\n[full tool output could not be stored securely: {error}]")
            });
        self.record_result(&call, turn_id, &presented, outcome.ok)
            .await?;
        let desktop_screenshot = if outcome.ok && call.name.eq_ignore_ascii_case("desktop") {
            serde_json::from_str::<Value>(&outcome.content)
                .ok()
                .and_then(|result| crate::desktop::screenshot_path(&result))
        } else {
            None
        };
        Ok(CompletedToolCall {
            touched: outcome.touched,
            desktop_screenshot,
        })
    }

    async fn approved(&self, call: &ToolCall, tool: &Arc<dyn Tool>, summary: &str) -> Result<bool> {
        let definition = tool.definition();
        if definition.approval == ApprovalRequirement::None
            || (definition.approval == ApprovalRequirement::Standard
                && self.approval == ApprovalPolicy::Never)
        {
            return Ok(true);
        }
        if definition.approval == ApprovalRequirement::Standard
            && self.always_allow.lock().await.iter().any(|s| s == summary)
        {
            return Ok(true);
        }

        let reason = match definition.approval {
            ApprovalRequirement::Privileged => format!(
                "`{}` requests root privileges; full-access never implies root",
                call.name
            ),
            ApprovalRequirement::Standard => {
                format!("`{}` writes or executes with normal OS access", call.name)
            }
            ApprovalRequirement::None => unreachable!(),
        };
        self.request_approval(
            call.id.clone(),
            summary.to_string(),
            reason,
            definition.approval == ApprovalRequirement::Standard,
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
            true,
        )
        .await
    }

    async fn request_approval(
        &self,
        call_id: String,
        command: String,
        reason: String,
        allow_always: bool,
    ) -> Result<bool> {
        let _ = self
            .events
            .send(Event::ApprovalRequest {
                call_id: call_id.clone(),
                command: command.clone(),
                cwd: self.workspace.clone(),
                reason,
                allow_always,
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
                    if allow_always {
                        self.always_allow.lock().await.push(command);
                    }
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

    fn record_desktop_screenshot(&self, path: &std::path::Path) -> Result<()> {
        let content = crate::desktop::screenshot_user_content(path)?;
        let msg = Message::User {
            content: content.clone(),
        };
        self.store.append_turn(
            &self.session_id,
            "user",
            &serde_json::to_string(&msg)?,
            estimate("desktop screenshot"),
            false,
        )?;
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
            let compact_content = if turn.role == "user" {
                crate::provider::user_content_for_display(&turn.content)
            } else {
                turn.content.clone()
            };
            let content: String = compact_content.chars().take(4_000).collect();
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

    fn clone_handle(&self) -> Self {
        Self {
            provider: self.provider.clone(),
            registry: self.registry.clone(),
            store: self.store.clone(),
            session_id: self.session_id.clone(),
            model: self.model.clone(),
            approval: self.approval,
            context_budget: self.context_budget,
            workspace: self.workspace.clone(),
            verify_policy: self.verify_policy.clone(),
            output_store: self.output_store.clone(),
            events: self.events.clone(),
            approvals: self.approvals.clone(),
            always_allow: self.always_allow.clone(),
        }
    }
}

pub(crate) fn automatic_session_title(text: &str) -> String {
    let compact = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("Conversație nouă")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut chars = compact.chars();
    let mut title = chars.by_ref().take(64).collect::<String>();
    if chars.next().is_some() {
        title.push('…');
    }
    title
}

#[cfg(test)]
mod title_tests {
    use super::automatic_session_title;

    #[test]
    fn first_user_line_becomes_a_compact_title() {
        assert_eq!(
            automatic_session_title("  Repară   bara providerului\nmai multe detalii"),
            "Repară bara providerului"
        );
        assert!(automatic_session_title(&"ă".repeat(100)).ends_with('…'));
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
        "sudo" => format!("sudo {}", v["command"].as_str().unwrap_or("?")),
        "apply_patch" => {
            let n = v["patch"].as_str().unwrap_or("").matches("*** ").count();
            format!("patch touching {n} file(s)")
        }
        "read_file" => format!("read {}", v["path"].as_str().unwrap_or("?")),
        "read_tool_output" => format!(
            "read stored tool output {}",
            v["handle"].as_str().unwrap_or("?")
        ),
        "search" => format!("search {:?}", v["pattern"].as_str().unwrap_or("?")),
        "desktop" => match v["action"].as_str().unwrap_or("observe") {
            "click" | "double_click" | "move" => format!(
                "desktop {} at {},{}",
                v["action"].as_str().unwrap_or("action"),
                v["x"].as_i64().unwrap_or(-1),
                v["y"].as_i64().unwrap_or(-1)
            ),
            action => format!("desktop {action}"),
        },
        "activate_skill" => format!("activate skill {}", v["name"].as_str().unwrap_or("?")),
        "read_skill_resource" => format!(
            "read skill resource {}/{}",
            v["skill"].as_str().unwrap_or("?"),
            v["path"].as_str().unwrap_or("?")
        ),
        other => other.to_string(),
    }
}
