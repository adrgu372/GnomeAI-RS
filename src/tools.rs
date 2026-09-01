use std::{
    fs,
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, anyhow, bail};
use futures_util::{StreamExt, future::join_all};
use regex::Regex;
use serde_json::{Map, Value, json};
use tokio::{
    io::AsyncReadExt,
    process::Command,
    sync::{RwLock, oneshot},
    time::{sleep, timeout},
};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use uuid::Uuid;

use crate::{
    config::AppConfig,
    consistency::{ToolObservation, enforce_final_answer},
    desktop,
    firecrawl::{firecrawl_fetch, firecrawl_search},
    llama::{ChatStreamEvent, LlamaClient, LlamaResponse},
    memory::append_memory_block,
    node_protocol::QueueJobRequest,
    nodes,
    privilege::{
        clear_keyring_secret, command_requests_privilege, keyring_available, lookup_keyring_secret,
        store_keyring_secret, validate_sudo,
    },
    protocol::Event,
    provider_catalog::{AuthKind, WireProtocol, preset},
    questions::PendingQuestions,
    runtime::RuntimeHandles,
    runtime_profile::{RuntimeProfile, build_runtime_aware_system_prompt},
    sandbox::{SandboxMode, SandboxPolicy, sandboxed_command, spawn_sandboxed_with_cancel},
    skills,
    storage::AppPaths,
    tasks,
    turn_stream::TurnStream,
    vision::SYSTEM_PROMPT,
    web_approvals::PendingApprovals,
};

/// Ceiling on the serialized conversation carried between tool rounds.
///
/// Rounds are unbounded, so something else has to stop a loop that keeps
/// producing large tool results. Sized to leave room under a 128k-token
/// context after the system prompt and tool schemas.
const MAX_LOOP_CONTEXT_BYTES: usize = 256 * 1024;

/// How many trailing messages stay verbatim when the loop compacts.
const COMPACTION_KEEP_RECENT: usize = 6;

/// Ceiling on the generated summary, so folding cannot grow the context.
const MAX_SUMMARY_BYTES: usize = 6 * 1024;

/// Cheap stand-in for a token count. Every provider ships a different
/// tokenizer, and this only has to be right to within a factor of two.
fn context_bytes(messages: &[Value]) -> usize {
    messages
        .iter()
        .map(|message| message.to_string().len())
        .sum()
}

fn message_role(message: &Value) -> &str {
    message["role"].as_str().unwrap_or_default()
}

/// The stretch of conversation to fold into one summary.
#[derive(Debug, PartialEq, Eq)]
struct MessageCompaction {
    range: std::ops::Range<usize>,
    freed_bytes: usize,
}

/// Decide what to fold away. `None` means there is nothing safe to do.
///
/// Two rules that are not optional, both inherited from the terminal agent's
/// store:
///
/// 1. Never cut between an assistant message carrying tool calls and its
///    matching tool results. Every provider rejects a dangling tool call, and
///    the error message is never about that. The cut walks backwards until no
///    tool result is left orphaned at the head of what survives.
///
/// 2. The seed survives — the system prompt and the original request.
///    Summarise the goal away and the model starts confidently solving a
///    different problem.
fn plan_message_compaction(
    messages: &[Value],
    budget_bytes: usize,
    keep_recent: usize,
) -> Option<MessageCompaction> {
    if context_bytes(messages) <= budget_bytes {
        return None;
    }
    // Everything before the first assistant reply is the seed: the system
    // prompt and the request being worked on.
    let pinned = messages
        .iter()
        .position(|message| message_role(message) == "assistant")
        .unwrap_or(messages.len());

    let mut cut = messages.len().saturating_sub(keep_recent);
    // A tool result must never be the first surviving message after a cut.
    while cut > pinned && message_role(&messages[cut]) == "tool" {
        cut -= 1;
    }
    // Folding one message into one summary frees nothing and would let the
    // loop spin: insist on real progress.
    if cut < pinned + 2 {
        return None;
    }

    let range = pinned..cut;
    let freed_bytes = context_bytes(&messages[range.clone()]);
    Some(MessageCompaction { range, freed_bytes })
}

/// Replace a stretch of the conversation with a model-written summary.
///
/// The summary enters as a `user` message so the model reads it as external
/// context about what already happened, rather than as its own earlier claim.
async fn compact_messages(
    client: &LlamaClient,
    cfg: &AppConfig,
    model: &str,
    messages: &mut Vec<Value>,
    plan: MessageCompaction,
) {
    let mut source = String::new();
    for message in &messages[plan.range.clone()] {
        let role = message_role(message);
        let content = message["content"].as_str().unwrap_or_default();
        let tools = message["tool_calls"]
            .as_array()
            .map(|calls| {
                calls
                    .iter()
                    .filter_map(|call| call["function"]["name"].as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .filter(|names| !names.is_empty())
            .map(|names| format!(" [tools: {names}]"))
            .unwrap_or_default();
        source.push_str(&format!(
            "{role}{tools}: {}\n\n",
            content.chars().take(4_000).collect::<String>()
        ));
        if source.len() >= 32_000 {
            source.push_str("\n[older content truncated]");
            break;
        }
    }

    let request = vec![
        json!({"role": "system", "content":
            "Summarize the conversation state for another tool-using turn. Preserve the \
             user's goal, decisions taken, files inspected or changed, tool results, errors, \
             and unfinished work. Be concise and factual."}),
        json!({"role": "user", "content": source.clone()}),
    ];
    let summary = match client.chat(cfg, model, request, 0.2).await {
        Ok(response) if !response.content.trim().is_empty() => response.content,
        // A failed summary must not lose the history outright: keep a truncated
        // transcript rather than pretending those rounds never happened.
        _ => format!(
            "Compacted earlier rounds (model summary unavailable):\n{}",
            source
        ),
    };
    let summary: String = summary.chars().take(MAX_SUMMARY_BYTES).collect();

    messages.splice(
        plan.range,
        [json!({
            "role": "user",
            "content": format!("[compacted earlier tool rounds]\n{summary}"),
        })],
    );
}

#[derive(Debug, Clone, Copy)]
struct ToolMeta {
    name: &'static str,
    description: &'static str,
    search_hint: &'static str,
    aliases: &'static [&'static str],
}

#[allow(clippy::too_many_arguments)]
pub async fn run_tool_loop(
    client: &LlamaClient,
    cfg: &AppConfig,
    paths: &AppPaths,
    model: &str,
    system_prompt: &str,
    runtime_profile: &RuntimeProfile,
    memory_block: Option<&str>,
    user_prompt: &str,
    session_key: Option<&str>,
    pending_questions: &PendingQuestions,
    pending_approvals: &PendingApprovals,
    runtime: &RuntimeHandles,
    config_state: Option<Arc<RwLock<AppConfig>>>,
    agent_depth: u32,
    local_web: bool,
    turn: &TurnStream,
) -> String {
    run_tool_loop_internal(
        client,
        cfg,
        paths,
        model,
        system_prompt,
        runtime_profile,
        memory_block,
        user_prompt,
        session_key,
        pending_questions,
        pending_approvals,
        runtime,
        config_state,
        agent_depth,
        local_web,
        None,
        AgentProfile::Root,
        turn,
    )
    .await
}

/// One model round, streamed when the provider can do it.
///
/// Falls back to the buffered call for wire protocols with no streaming tool
/// format, so nothing regresses where streaming is impossible. A mid-stream
/// failure keeps whatever already reached the user instead of discarding the
/// round: partial text the browser has already drawn is worth more than a
/// clean error nobody can act on.
async fn stream_model_round(
    client: &LlamaClient,
    cfg: &AppConfig,
    model: &str,
    messages: Vec<Value>,
    schemas: Vec<Value>,
    turn: &TurnStream,
) -> anyhow::Result<LlamaResponse> {
    let opened = client
        .chat_stream_with_tools(
            cfg,
            model,
            messages.clone(),
            0.3,
            schemas.clone(),
            Some(json!("auto")),
        )
        .await;
    let mut stream = match opened {
        Ok(stream) => stream,
        Err(error) => {
            info!("Streaming round unavailable, using the buffered call: {error}");
            return client
                .chat_with_tools(cfg, model, messages, 0.3, schemas, Some(json!("auto")))
                .await;
        }
    };

    let mut content = String::new();
    let mut tool_calls = Vec::new();
    let mut failure = None;

    loop {
        let next = tokio::select! {
            biased;
            // Cancellation wins the race deliberately: an interrupt that waits
            // for the next token is not an interrupt.
            _ = turn.cancel_token().cancelled() => None,
            next = stream.next() => next,
        };
        let Some(event) = next else { break };
        match event {
            Ok(ChatStreamEvent::Text(text)) => {
                content.push_str(&text);
                turn.emit(Event::Token { text }).await;
            }
            Ok(ChatStreamEvent::Reasoning(text)) => {
                turn.emit(Event::Reasoning { text }).await;
            }
            Ok(ChatStreamEvent::ToolCalls(calls)) => tool_calls = calls,
            Err(error) => {
                failure = Some(error);
                break;
            }
        }
    }

    if let Some(error) = failure {
        if content.trim().is_empty() && tool_calls.is_empty() {
            return Err(error);
        }
        warn!("Streaming round ended early, keeping partial output: {error}");
    }

    Ok(LlamaResponse {
        content,
        tool_calls,
    })
}

/// One line the user can judge without reading JSON. A progress row is only as
/// useful as this string.
fn summarize_call(name: &str, args: &Map<String, Value>) -> String {
    let text = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or("?");
    match name {
        "Bash" => short_command(text("command"), 120),
        "Sudo" => format!("sudo {}", short_command(text("command"), 110)),
        "Read" => format!("read {}", text("path")),
        "Write" => format!("write {}", text("path")),
        "Edit" => format!("edit {}", text("path")),
        "Glob" => format!("glob {}", text("pattern")),
        "Grep" => format!("grep {:?}", text("pattern")),
        "WebSearch" => format!("search {:?}", text("query")),
        "WebFetch" => format!("fetch {}", text("url")),
        "Desktop" => match text("action") {
            "click" | "double_click" | "move" => format!(
                "desktop {} at {},{}",
                text("action"),
                args.get("x").and_then(Value::as_i64).unwrap_or(-1),
                args.get("y").and_then(Value::as_i64).unwrap_or(-1)
            ),
            "inspect" => format!("inspect desktop UI {:?}", text("query")),
            "activate" | "set_text" | "focus" => {
                format!("desktop {} semantic target", text("action"))
            }
            action => format!("desktop {action}"),
        },
        "Node" => match text("action") {
            "list" => "list remote nodes".into(),
            "exec" => format!(
                "run on {}: {}",
                text("node_id"),
                short_command(text("command"), 100)
            ),
            action => format!("node {action}"),
        },
        "Agent" => format!("delegate: {}", text("description")),
        "Skill" => format!("skill {}", text("name")),
        "Learn" => format!("learn skill {}", text("name")),
        "RunSkill" => format!("run skill {} on {}", text("name"), text("target")),
        "Config" => format!("config {}", text("setting")),
        "TaskOutput" => format!("read output {}", text("taskId")),
        other => other.to_string(),
    }
}

async fn run_tool_loop_internal(
    client: &LlamaClient,
    cfg: &AppConfig,
    paths: &AppPaths,
    model: &str,
    system_prompt: &str,
    runtime_profile: &RuntimeProfile,
    memory_block: Option<&str>,
    user_prompt: &str,
    session_key: Option<&str>,
    pending_questions: &PendingQuestions,
    pending_approvals: &PendingApprovals,
    runtime: &RuntimeHandles,
    config_state: Option<Arc<RwLock<AppConfig>>>,
    agent_depth: u32,
    local_web: bool,
    agent_id: Option<&str>,
    agent_profile: AgentProfile,
    turn: &TurnStream,
) -> String {
    let whatsapp_origin = session_key.is_some_and(is_whatsapp_scope);
    let runtime_aware_system_prompt = append_memory_block(
        &build_runtime_aware_system_prompt(system_prompt, runtime_profile),
        memory_block,
    );
    let channel_execution_policy = if whatsapp_origin {
        "WhatsApp authorization: this turn came from an allowed WhatsApp chat. The user's inbound message itself authorizes the standard user-level tool calls needed to fulfill it, so execute them without waiting for a desktop confirmation. Read-only mode still blocks mutations. Sudo may proceed only with an existing sudo ticket or a valid credential already stored in the local keyring; if neither is available, fail clearly instead of opening a desktop prompt."
    } else {
        "Native desktop authorization: obey the selected execution mode and request local confirmation whenever normal mode requires it."
    };
    let runtime_aware_system_prompt = format!(
        "{runtime_aware_system_prompt}\n\nSelected workspace: {}\nShared desktop/WhatsApp execution mode: {}.\n{channel_execution_policy}\n{}\nFor graphical tasks, navigate autonomously with Desktop. Start with semantic `inspect`, then use the returned target with `activate`, `set_text`, or `focus`; these actions return an updated tree without an image round-trip. Use `observe` and coordinates only when the needed element is absent from AT-SPI, and never guess coordinates without a current capture.\nUse Node to inspect and operate paired weak devices. Use Learn only after an explicit user request to retain a reusable workflow; learning never authorizes or runs the entrypoint. RunSkill is a separate approved operation.\nWhen a tool is needed, call it through the native tool-calling API. Never print a tool call as markdown such as `**WebSearch** (query: ...)`. After receiving tool results, answer the user directly instead of announcing another intermediate step.",
        paths.workspace_dir.display(),
        cfg.web_sandbox_mode,
        agent_profile.guidance(),
    );
    let mut messages = vec![
        json!({"role": "system", "content": runtime_aware_system_prompt}),
        json!({"role": "user", "content": user_prompt}),
    ];
    // `0` means unlimited; anything else is a safety valve, not a work limit.
    let step_cap = cfg.tool_loop_max_steps;
    let schemas = tool_schemas_for(agent_profile, whatsapp_origin);
    let mut final_content = String::new();
    let mut structured_output_only = false;
    let mut tool_observations = Vec::new();
    let mut exhausted_steps = true;
    let session_key = session_key
        .map(normalize_ws)
        .filter(|item| !item.is_empty())
        .unwrap_or_else(|| "default".into());
    let tool_ctx = ToolContext {
        approval_scope: approval_scope_for(&session_key, agent_id),
        session_key,
        agent_depth,
        agent_id: agent_id.map(str::to_string),
        agent_profile,
        memory_block: memory_block
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(str::to_string),
    };

    let mut step = 0u32;
    loop {
        step += 1;
        // A fixed round cap ends useful work mid-task, which is why the
        // terminal agent does not have one. What actually has to be bounded is
        // context, not rounds: an unbounded loop grows `messages` until the
        // provider rejects the request with an error that never mentions why.
        // So the loop runs free and these two budgets end it instead.
        if step_cap > 0 && step > step_cap {
            break;
        }
        match plan_message_compaction(&messages, MAX_LOOP_CONTEXT_BYTES, COMPACTION_KEEP_RECENT) {
            Some(plan) => {
                let freed = plan.freed_bytes;
                info!(
                    "Compacting {} message(s) after {} round(s)",
                    plan.range.len(),
                    step - 1
                );
                compact_messages(client, cfg, model, &mut messages, plan).await;
                turn.emit(Event::Compacted {
                    freed_tokens: (freed / 4) as i64,
                })
                .await;
            }
            // Over budget with nothing safe to fold — the recent rounds alone
            // fill the window. Stop and synthesise instead of sending a request
            // the provider will reject.
            None if context_bytes(&messages) > MAX_LOOP_CONTEXT_BYTES => {
                warn!("Context budget exceeded with nothing safe to compact");
                break;
            }
            None => {}
        }
        if turn.is_cancelled() {
            exhausted_steps = false;
            break;
        }
        let response =
            match stream_model_round(client, cfg, model, messages.clone(), schemas.clone(), turn)
                .await
            {
                Ok(response) => response,
                Err(err) => {
                    warn!("Tool loop unavailable, falling back to plain chat: {err}");
                    return match client.chat(cfg, model, messages, 0.3).await {
                        Ok(response) if !response.content.trim().is_empty() => {
                            enforce_final_answer(
                                &response.content,
                                runtime_profile,
                                &tool_observations,
                            )
                        }
                        Ok(_) => "[Empty response]".into(),
                        Err(err) => format!("[LLM error: {err}]"),
                    };
                }
            };
        if turn.is_cancelled() {
            final_content = response.content;
            exhausted_steps = false;
            break;
        }

        let mut normalized_calls = response
            .tool_calls
            .iter()
            .filter_map(normalize_tool_call)
            .collect::<Vec<_>>();
        if normalized_calls.is_empty() {
            normalized_calls = parse_textual_tool_calls(&response.content);
        }
        if normalized_calls.is_empty() {
            final_content = unwrap_textual_assistant_reply(&response.content);
            exhausted_steps = false;
            break;
        }

        messages.push(json!({
            "role": "assistant",
            "content": content_before_textual_tool_call(&response.content),
            "tool_calls": normalized_calls,
        }));

        let parsed_calls = normalized_calls
            .iter()
            .map(|tool_call| {
                let function = tool_call.get("function").and_then(Value::as_object);
                let name = function
                    .and_then(|item| item.get("name"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let args = function
                    .and_then(|item| item.get("arguments"))
                    .and_then(Value::as_str)
                    .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                    .and_then(|value| value.as_object().cloned())
                    .unwrap_or_default();
                let call_id = tool_call
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("call")
                    .to_string();
                (name, args, call_id)
            })
            .collect::<Vec<_>>();

        // Claude Code-style fan-out: independent Agent calls emitted in one
        // assistant response are polled together. Other tool batches stay
        // ordered because writes and shell commands can depend on each other.
        let parallel_agent_batch =
            parsed_calls.len() > 1 && parsed_calls.iter().all(|(name, _, _)| name == "Agent");
        let executions = if parallel_agent_batch {
            join_all(parsed_calls.iter().map(|(name, args, call_id)| {
                execute_reported_call(
                    name,
                    args,
                    call_id,
                    cfg,
                    paths,
                    client,
                    model,
                    &tool_ctx,
                    runtime_profile,
                    pending_questions,
                    pending_approvals,
                    runtime,
                    config_state.clone(),
                    local_web,
                    turn,
                )
            }))
            .await
        } else {
            let mut results = Vec::with_capacity(parsed_calls.len());
            for (name, args, call_id) in &parsed_calls {
                if turn.is_cancelled() {
                    // Report the rest as skipped rather than silently dropping
                    // them: the model still needs a result for every call it
                    // made, or the next request is malformed.
                    results.push(Err(anyhow!("interrupted by the user")));
                    continue;
                }
                results.push(
                    execute_reported_call(
                        name,
                        args,
                        call_id,
                        cfg,
                        paths,
                        client,
                        model,
                        &tool_ctx,
                        runtime_profile,
                        pending_questions,
                        pending_approvals,
                        runtime,
                        config_state.clone(),
                        local_web,
                        turn,
                    )
                    .await,
                );
            }
            results
        };

        let mut structured_output = None;
        let mut desktop_screenshots = Vec::new();
        for ((name, args, call_id), execution) in parsed_calls.iter().zip(executions) {
            let output = match execution {
                Ok(result) => {
                    if name == "Desktop"
                        && let Some(path) = desktop::screenshot_path(&result)
                    {
                        desktop_screenshots.push(path);
                    }
                    let observation =
                        ToolObservation::from_success(runtime_profile, name, args, &result);
                    let source_attribution = observation.source_attribution.as_json();
                    let observation_json = observation.as_json_for_model();
                    tool_observations.push(observation);
                    if name == "StructuredOutput" {
                        structured_output = Some(
                            serde_json::to_string_pretty(
                                result
                                    .get("structured_output")
                                    .unwrap_or(&Value::Object(args.clone())),
                            )
                            .unwrap_or_else(|_| "{}".into()),
                        );
                    }
                    json!({
                        "ok": true,
                        "tool": name,
                        "source_attribution": source_attribution,
                        "tool_observation": observation_json,
                        "result": result,
                    })
                }
                Err(err) => {
                    let observation =
                        ToolObservation::from_error(runtime_profile, name, args, &err.to_string());
                    let source_attribution = observation.source_attribution.as_json();
                    let observation_json = observation.as_json_for_model();
                    tool_observations.push(observation);
                    json!({
                        "ok": false,
                        "tool": name,
                        "source_attribution": source_attribution,
                        "tool_observation": observation_json,
                        "error": err.to_string(),
                    })
                }
            };

            messages.push(json!({
                "role": "tool",
                "tool_call_id": call_id,
                "content": output.to_string(),
            }));
        }

        // Preserve provider tool-call ordering, then add each fresh screen as
        // a real multimodal user turn. Both OpenAI and Anthropic encoders can
        // now let the model inspect pixels before choosing the next action.
        for path in desktop_screenshots {
            match desktop::screenshot_user_content(&path)
                .and_then(|content| serde_json::from_str::<Value>(&content).map_err(Into::into))
            {
                Ok(content) => messages.push(json!({"role": "user", "content": content})),
                Err(error) => warn!(%error, "could not attach desktop screenshot to model round"),
            }
        }

        final_content = response.content;
        if structured_output.is_some()
            && normalized_calls.iter().all(|call| {
                call.get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
                    == Some("StructuredOutput")
            })
        {
            final_content = structured_output.unwrap_or_default();
            structured_output_only = true;
            exhausted_steps = false;
            break;
        }
    }

    if exhausted_steps && !structured_output_only {
        messages.push(json!({
            "role": "user",
            "content": "Tool round limit reached. Do not call or describe any more tools. Answer the original user request now using the tool results already present above. Return only the final answer."
        }));
        final_content = match client.chat(cfg, model, messages, 0.2).await {
            Ok(response) => unwrap_textual_assistant_reply(&response.content),
            Err(err) => {
                warn!("Final synthesis after tool round limit failed: {err}");
                String::new()
            }
        };
    }

    let trimmed = final_content.trim();
    if trimmed.is_empty() {
        empty_response_fallback(&tool_observations).unwrap_or_else(|| "[Empty response]".into())
    } else if structured_output_only {
        trimmed.to_string()
    } else {
        let enforced = enforce_final_answer(trimmed, runtime_profile, &tool_observations);
        if enforced.trim().is_empty() {
            empty_response_fallback(&tool_observations).unwrap_or_else(|| "[Empty response]".into())
        } else {
            enforced
        }
    }
}

fn empty_response_fallback(tool_observations: &[ToolObservation]) -> Option<String> {
    let successful = tool_observations
        .iter()
        .filter(|item| item.success)
        .collect::<Vec<_>>();
    if !successful.is_empty() {
        let details = successful
            .into_iter()
            .rev()
            .take(3)
            .map(|item| format!("- {}", item.summary))
            .collect::<Vec<_>>()
            .join("\n");
        return Some(format!(
            "The model did not return a final answer after the tools ran. Useful observations:\n{details}"
        ));
    }

    let failed = tool_observations
        .iter()
        .filter(|item| !item.success)
        .rev()
        .take(2)
        .map(|item| format!("- {}", item.summary))
        .collect::<Vec<_>>();
    if failed.is_empty() {
        None
    } else {
        Some(format!(
            "The model did not produce a final answer. The latest observed errors were:\n{}",
            failed.join("\n")
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentProfile {
    Root,
    GeneralPurpose,
    Explore,
    Plan,
}

impl AgentProfile {
    fn from_subagent_type(value: &str) -> Self {
        match normalize_ws(value).to_ascii_lowercase().as_str() {
            "explore" | "research" | "search" => Self::Explore,
            "plan" | "planner" => Self::Plan,
            _ => Self::GeneralPurpose,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Root => "root",
            Self::GeneralPurpose => "general-purpose",
            Self::Explore => "Explore",
            Self::Plan => "Plan",
        }
    }

    fn allows(self, tool: &str) -> bool {
        match self {
            Self::Root => true,
            Self::GeneralPurpose => !matches!(tool, "AskUserQuestion" | "Config" | "Learn"),
            Self::Explore => matches!(
                tool,
                "ToolSearch"
                    | "Read"
                    | "Glob"
                    | "Grep"
                    | "Skill"
                    | "StructuredOutput"
                    | "TaskGet"
                    | "TaskList"
                    | "TaskOutput"
                    | "WebSearch"
                    | "WebFetch"
            ),
            Self::Plan => matches!(
                tool,
                "Agent"
                    | "ToolSearch"
                    | "Read"
                    | "Glob"
                    | "Grep"
                    | "Skill"
                    | "StructuredOutput"
                    | "TodoWrite"
                    | "TaskCreate"
                    | "TaskGet"
                    | "TaskList"
                    | "TaskUpdate"
                    | "TaskOutput"
                    | "WebSearch"
                    | "WebFetch"
            ),
        }
    }

    fn guidance(self) -> &'static str {
        match self {
            Self::Root => {
                "For complex work, use the Agent tool proactively to delegate independent, well-scoped tasks. Give every subagent all context it needs because it receives a separate conversation. Launch multiple independent Agent calls in the same response so they can run concurrently. Use Explore for read-only codebase/web investigation, Plan for solution design without file changes, and general-purpose for implementation. Each Agent call may independently choose provider_id and model; use inherit unless the user requested a particular provider/model or a saved provider is clearly better suited. Do not delegate trivial work or duplicate work already in progress. Synchronous subagent results return directly; background agents are checked with TaskOutput."
            }
            Self::GeneralPurpose => {
                "You are a general-purpose subagent with an isolated context. You may inspect and modify the shared workspace using the available tools. Do not ask the human questions; report blockers to the parent."
            }
            Self::Explore => {
                "You are an Explore subagent with an isolated, read-only context. Search broadly, read concrete sources, and return file paths, symbols, and evidence. You cannot modify files or execute shell commands."
            }
            Self::Plan => {
                "You are a Plan subagent with an isolated, read-only context. Investigate first, then return an actionable implementation plan with affected files, risks, and verification. You may delegate read-only exploration but cannot modify files or execute shell commands."
            }
        }
    }
}

#[derive(Debug)]
struct ToolContext {
    /// The conversation. Durable state — tasks, todos, subagent registrations —
    /// is filed under this so a subagent's work still shows up in the chat that
    /// started it.
    session_key: String,
    /// Who is waiting on an approval dialog. Distinct from `session_key`
    /// because `PendingApprovals` allows one outstanding request per scope, so
    /// sharing it would make concurrent subagents cancel each other's prompts.
    approval_scope: String,
    agent_depth: u32,
    agent_id: Option<String>,
    agent_profile: AgentProfile,
    memory_block: Option<String>,
}

/// One outstanding approval is allowed per scope. The conversation is the right
/// unit for that when a single agent is working, but a fan-out of subagents
/// runs several tool loops against one conversation at once — so each subagent
/// gets its own scope, keyed by the task id it already owns.
fn approval_scope_for(session_key: &str, agent_id: Option<&str>) -> String {
    match agent_id {
        Some(agent_id) => format!("{session_key}#{agent_id}"),
        None => session_key.to_string(),
    }
}

/// Runs one tool call and brackets it with the progress events the browser
/// draws. Wrapping rather than emitting inside `execute_tool_call` keeps the
/// dispatcher free of presentation concerns and guarantees that every started
/// row also gets an ended row, including on the error paths.
#[allow(clippy::too_many_arguments)]
async fn execute_reported_call(
    name: &str,
    args: &Map<String, Value>,
    call_id: &str,
    cfg: &AppConfig,
    paths: &AppPaths,
    client: &LlamaClient,
    model: &str,
    tool_ctx: &ToolContext,
    runtime_profile: &RuntimeProfile,
    pending_questions: &PendingQuestions,
    pending_approvals: &PendingApprovals,
    runtime: &RuntimeHandles,
    config_state: Option<Arc<RwLock<AppConfig>>>,
    local_web: bool,
    turn: &TurnStream,
) -> anyhow::Result<Value> {
    turn.emit(Event::ToolCallStarted {
        call_id: call_id.to_string(),
        name: name.to_string(),
        summary: summarize_call(name, args),
    })
    .await;

    let started = std::time::Instant::now();
    let outcome = execute_tool_call(
        name,
        args,
        cfg,
        paths,
        client,
        model,
        tool_ctx,
        runtime_profile,
        pending_questions,
        pending_approvals,
        runtime,
        config_state,
        local_web,
        turn,
    )
    .await;

    turn.emit(Event::ToolCallEnded {
        call_id: call_id.to_string(),
        ok: outcome.is_ok(),
        duration_ms: started.elapsed().as_millis() as u64,
    })
    .await;
    outcome
}

#[allow(clippy::too_many_arguments)]
async fn execute_tool_call(
    name: &str,
    args: &Map<String, Value>,
    cfg: &AppConfig,
    paths: &AppPaths,
    client: &LlamaClient,
    model: &str,
    tool_ctx: &ToolContext,
    runtime_profile: &RuntimeProfile,
    pending_questions: &PendingQuestions,
    pending_approvals: &PendingApprovals,
    runtime: &RuntimeHandles,
    config_state: Option<Arc<RwLock<AppConfig>>>,
    local_web: bool,
    turn: &TurnStream,
) -> anyhow::Result<Value> {
    info!("Tool call: {name} [scope={}]", tool_ctx.session_key);
    if !tool_ctx.agent_profile.allows(name) {
        bail!(
            "Tool {name} is not available to the {} subagent profile",
            tool_ctx.agent_profile.label()
        );
    }
    match name {
        // AskUserQuestion is a browser widget: waiting here would leave a
        // WhatsApp turn apparently frozen for its one-hour default timeout.
        // It is also removed from the advertised WhatsApp schemas, but keep
        // this guard for providers that emit textual/unadvertised tool calls.
        "AskUserQuestion" if is_whatsapp_scope(&tool_ctx.session_key) => Ok(json!({
            "channel": "whatsapp",
            "interactive_widget_available": false,
            "questions": args.get("questions").cloned().unwrap_or_else(|| json!([])),
            "instruction": "Ask these questions directly in the WhatsApp reply, then stop and wait for the user's next message. Do not call AskUserQuestion again."
        })),
        "AskUserQuestion" => {
            tool_ask_user_question(pending_questions, &tool_ctx.session_key, args).await
        }
        "Agent" => {
            tool_agent(
                cfg,
                paths,
                client,
                model,
                args,
                tool_ctx,
                runtime_profile,
                pending_questions,
                pending_approvals,
                runtime,
                config_state.clone(),
                local_web,
                turn,
            )
            .await
        }
        "ToolSearch" => tool_search(args, tool_ctx.agent_profile),
        "Read" => tool_read(paths, args).await,
        "Write" => {
            authorize_standard(
                cfg,
                pending_approvals,
                &tool_ctx.approval_scope,
                "Write",
                &required_string(args, "path")?,
                "The tool will create or overwrite a workspace file.",
                paths,
            )
            .await?;
            tool_write(paths, args).await
        }
        "Edit" => {
            authorize_standard(
                cfg,
                pending_approvals,
                &tool_ctx.approval_scope,
                "Edit",
                &required_string(args, "path")?,
                "The tool will modify an existing workspace file.",
                paths,
            )
            .await?;
            tool_edit(paths, args).await
        }
        "Glob" => tool_glob(paths, args),
        "Grep" => tool_grep(paths, args).await,
        "Bash" => {
            tool_bash(
                cfg,
                paths,
                args,
                runtime,
                &tool_ctx.session_key,
                &tool_ctx.approval_scope,
                pending_approvals,
                turn,
            )
            .await
        }
        "Desktop" => {
            let action = value_string(args, "action").unwrap_or_else(|| "observe".into());
            authorize_standard(
                cfg,
                pending_approvals,
                &tool_ctx.approval_scope,
                "Desktop",
                &format!("desktop {action}"),
                "The agent will inspect or control the current graphical desktop session.",
                paths,
            )
            .await?;
            desktop::perform(&paths.generated_dir, args, turn.cancel_token()).await
        }
        "Node" => {
            let action = value_string(args, "action").unwrap_or_else(|| "list".into());
            if action != "list" {
                let node_id = value_string(args, "node_id").unwrap_or_else(|| "?".into());
                authorize_standard(
                    cfg,
                    pending_approvals,
                    &tool_ctx.approval_scope,
                    "Node",
                    &format!("remote node {node_id}: {action}"),
                    "The agent will execute a command on the selected paired device.",
                    paths,
                )
                .await?;
            }
            let root_approved = action != "list"
                && matches!(
                    web_sandbox_mode(cfg),
                    SandboxMode::Normal | SandboxMode::IsolatedWorkspaceWrite
                )
                && !is_whatsapp_scope(&tool_ctx.approval_scope);
            tool_node(cfg, args, root_approved).await
        }
        "Sudo" => {
            tool_sudo(
                cfg,
                paths,
                args,
                &tool_ctx.approval_scope,
                pending_approvals,
                local_web,
                turn,
            )
            .await
        }
        "Config" => tool_config(cfg, paths, args, config_state).await,
        "Skill" => tool_skill(paths, args),
        "Learn" => {
            authorize_standard(
                cfg,
                pending_approvals,
                &tool_ctx.approval_scope,
                "Learn",
                &format!(
                    "learn skill {}",
                    value_string(args, "name").unwrap_or_default()
                ),
                "The agent will create or replace a persistent user-managed skill.",
                paths,
            )
            .await?;
            tool_learn(args).await
        }
        "RunSkill" => {
            tool_run_skill(cfg, paths, args, runtime, tool_ctx, pending_approvals, turn).await
        }
        "StructuredOutput" => Ok(json!({"structured_output": args})),
        "TodoWrite" => tasks::todo_write(
            paths,
            &tool_ctx.session_key,
            args.get("todos").unwrap_or(&Value::Array(Vec::new())),
        ),
        "TaskCreate" => tasks::task_create(paths, &tool_ctx.session_key, args),
        "TaskGet" => {
            let task_id = value_string(args, "taskId")
                .or_else(|| value_string(args, "task_id"))
                .ok_or_else(|| anyhow!("taskId is required"))?;
            tasks::task_get(paths, &tool_ctx.session_key, &task_id)
        }
        "TaskList" => tasks::task_list(paths, &tool_ctx.session_key),
        "TaskUpdate" => tasks::task_update(paths, &tool_ctx.session_key, args),
        "TaskStop" => {
            let task_id = value_string(args, "task_id")
                .or_else(|| value_string(args, "taskId"))
                .or_else(|| value_string(args, "shell_id"))
                .ok_or_else(|| anyhow!("task_id is required"))?;
            let mut response = tasks::task_stop(paths, &tool_ctx.session_key, &task_id)?;
            let runtime_stopped = runtime.stop(&task_id).await;
            if let Some(obj) = response.as_object_mut() {
                obj.insert("runtime_stopped".into(), json!(runtime_stopped));
            }
            Ok(response)
        }
        "TaskOutput" => tool_task_output(paths, &tool_ctx.session_key, args).await,
        "WebSearch" => tool_web_search(cfg, args).await,
        "WebFetch" => tool_web_fetch(cfg, client, model, args).await,
        other => bail!("Unknown tool: {other}"),
    }
}

fn normalize_tool_call(tool_call: &Value) -> Option<Value> {
    let obj = tool_call.as_object()?;
    let id = obj
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| {
            format!(
                "call_{}",
                Uuid::new_v4().simple().to_string()[..8].to_string()
            )
        });
    let function = obj.get("function").and_then(Value::as_object);
    let name = function
        .and_then(|item| item.get("name"))
        .or_else(|| obj.get("name"))
        .and_then(Value::as_str)?
        .trim()
        .to_string();
    if name.is_empty() {
        return None;
    }
    let raw_arguments = function
        .and_then(|item| item.get("arguments"))
        .or_else(|| obj.get("arguments"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let arguments = match raw_arguments {
        Value::String(text) => text,
        other => other.to_string(),
    };

    Some(json!({
        "id": id,
        "type": "function",
        "function": {
            "name": name,
            "arguments": arguments,
        }
    }))
}

/// Some local chat templates render a native tool decision as markdown prose
/// instead of populating `message.tool_calls`. Accept that narrow legacy shape
/// only for tools that are actually registered, so normal bold text cannot
/// accidentally execute anything.
fn parse_textual_tool_calls(content: &str) -> Vec<Value> {
    let trimmed = content.trim();
    let Some(after_open) = trimmed.strip_prefix("**") else {
        return Vec::new();
    };
    let Some((raw_name, rest)) = after_open.split_once("**") else {
        return Vec::new();
    };
    let Some(arguments) = rest
        .trim()
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
    else {
        return Vec::new();
    };
    let Some(name) = canonical_tool_name(raw_name.trim()) else {
        return Vec::new();
    };
    let args = parse_textual_arguments(arguments);
    vec![json!({
        "id": format!("call_{}", &Uuid::new_v4().simple().to_string()[..8]),
        "type": "function",
        "function": {
            "name": name,
            "arguments": Value::Object(args).to_string(),
        }
    })]
}

fn canonical_tool_name(candidate: &str) -> Option<&'static str> {
    tool_metadata().into_iter().find_map(|meta| {
        (meta.name.eq_ignore_ascii_case(candidate)
            || meta
                .aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(candidate)))
        .then_some(meta.name)
    })
}

fn parse_textual_arguments(input: &str) -> Map<String, Value> {
    let mut args = Map::new();
    for field in split_top_level(input, ',') {
        let Some((key, raw_value)) = split_once_top_level(field, ':') else {
            continue;
        };
        let key = key.trim().trim_matches(|ch| matches!(ch, '`' | '\'' | '"'));
        if key.is_empty() {
            continue;
        }
        args.insert(key.to_string(), parse_textual_value(raw_value.trim()));
    }
    args
}

fn parse_textual_value(raw: &str) -> Value {
    if raw.starts_with('"') && raw.ends_with('"') {
        return serde_json::from_str(raw).unwrap_or_else(|_| json!(raw.trim_matches('"')));
    }
    if (raw.starts_with('\'') && raw.ends_with('\''))
        || (raw.starts_with('`') && raw.ends_with('`'))
    {
        return json!(&raw[1..raw.len().saturating_sub(1)]);
    }
    serde_json::from_str(raw).unwrap_or_else(|_| json!(raw))
}

fn split_top_level(input: &str, separator: char) -> Vec<&str> {
    let mut fields = Vec::new();
    let mut start = 0;
    let mut quote = None;
    let mut escaped = false;
    let mut depth = 0u32;
    for (index, ch) in input.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if quote.is_some() && ch == '\\' {
            escaped = true;
            continue;
        }
        if matches!(ch, '"' | '\'' | '`') {
            if quote == Some(ch) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(ch);
            }
            continue;
        }
        if quote.is_some() {
            continue;
        }
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            _ if ch == separator && depth == 0 => {
                fields.push(input[start..index].trim());
                start = index + ch.len_utf8();
            }
            _ => {}
        }
    }
    fields.push(input[start..].trim());
    fields
}

fn split_once_top_level(input: &str, separator: char) -> Option<(&str, &str)> {
    let mut quote = None;
    let mut escaped = false;
    let mut depth = 0u32;
    for (index, ch) in input.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if quote.is_some() && ch == '\\' {
            escaped = true;
            continue;
        }
        if matches!(ch, '"' | '\'' | '`') {
            if quote == Some(ch) {
                quote = None;
            } else if quote.is_none() {
                quote = Some(ch);
            }
            continue;
        }
        if quote.is_some() {
            continue;
        }
        match ch {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            _ if ch == separator && depth == 0 => {
                return Some((input[..index].trim(), input[index + ch.len_utf8()..].trim()));
            }
            _ => {}
        }
    }
    None
}

fn content_before_textual_tool_call(content: &str) -> &str {
    let trimmed = content.trim();
    if parse_textual_tool_calls(trimmed).is_empty() {
        content
    } else {
        ""
    }
}

fn unwrap_textual_assistant_reply(content: &str) -> String {
    let trimmed = content.trim();
    let Some(rest) = trimmed.strip_prefix("**Assistant**") else {
        return content.to_string();
    };
    let Some(arguments) = rest
        .trim()
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
    else {
        return content.to_string();
    };
    parse_textual_arguments(arguments)
        .remove("reply")
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| content.to_string())
}

fn tool_search(args: &Map<String, Value>, profile: AgentProfile) -> anyhow::Result<Value> {
    let query = value_string(args, "query").unwrap_or_default();
    let max_results = value_u64(args, "max_results").unwrap_or(5).clamp(1, 20) as usize;
    let metas = tool_metadata()
        .into_iter()
        .filter(|meta| profile.allows(meta.name))
        .collect::<Vec<_>>();
    if query.trim().is_empty() {
        return Ok(json!({
            "matches": metas.iter().take(max_results).map(tool_meta_json).collect::<Vec<_>>(),
            "query": query,
            "total_tools": metas.len(),
        }));
    }
    let query_lower = query.to_lowercase();
    if let Some(selected) = query_lower.strip_prefix("select:") {
        let selected = selected.trim();
        let matches = metas
            .iter()
            .filter(|meta| {
                meta.name.eq_ignore_ascii_case(selected)
                    || meta
                        .aliases
                        .iter()
                        .any(|alias| alias.eq_ignore_ascii_case(selected))
            })
            .map(tool_meta_json)
            .collect::<Vec<_>>();
        return Ok(json!({"matches": matches, "query": query, "total_tools": metas.len()}));
    }

    let terms = query_lower.split_whitespace().collect::<Vec<_>>();
    let mut scored = Vec::new();
    for meta in &metas {
        let haystack = format!(
            "{} {} {} {}",
            meta.name,
            meta.description,
            meta.search_hint,
            meta.aliases.join(" ")
        )
        .to_lowercase();
        let mut score = 0;
        if meta.name.eq_ignore_ascii_case(&query)
            || meta
                .aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(&query))
        {
            score += 100;
        }
        for term in &terms {
            if meta.name.to_lowercase().contains(term)
                || meta
                    .aliases
                    .iter()
                    .any(|alias| alias.to_lowercase().contains(term))
            {
                score += 20;
            }
            if haystack.contains(term) {
                score += 5;
            }
        }
        if score > 0 {
            scored.push((score, *meta));
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(b.1.name)));
    Ok(json!({
        "matches": scored.into_iter().take(max_results).map(|(_, meta)| tool_meta_json(&meta)).collect::<Vec<_>>(),
        "query": query,
        "total_tools": metas.len(),
    }))
}

fn resolve_agent_provider(
    parent_cfg: &AppConfig,
    current_model: &str,
    requested_provider: &str,
    requested_model: Option<&str>,
) -> anyhow::Result<(AppConfig, String, String)> {
    let requested_provider = normalize_ws(requested_provider).to_ascii_lowercase();
    let inherit = requested_provider.is_empty()
        || matches!(
            requested_provider.as_str(),
            "inherit" | "current" | "parent"
        );
    let provider_id = if inherit {
        parent_cfg.provider_id.clone()
    } else {
        requested_provider
    };
    let provider =
        preset(&provider_id).ok_or_else(|| anyhow!("Unknown subagent provider: {provider_id}"))?;
    if provider.auth == AuthKind::Account {
        bail!(
            "WhatsApp subagents require an API provider; account-backed provider {} is available only in the native app",
            provider.name
        );
    }

    let mut agent_cfg = parent_cfg.clone();
    agent_cfg.provider_id = provider_id.clone();
    agent_cfg.provider_protocol = match provider.protocol {
        WireProtocol::Anthropic => "anthropic",
        WireProtocol::OpenAi => "openai",
        WireProtocol::CodexAppServer => "codex",
        WireProtocol::ClaudeCli => "claude-cli",
    }
    .into();
    if provider_id != "custom" || parent_cfg.provider_id != "custom" {
        agent_cfg.llama_base_url = provider.base_url.to_string();
    }
    let model = requested_model
        .map(normalize_ws)
        .filter(|model| !model.is_empty())
        .unwrap_or_else(|| {
            if inherit || provider_id == parent_cfg.provider_id {
                current_model.to_string()
            } else {
                provider.default_model.to_string()
            }
        });
    agent_cfg.default_model = model.clone();
    agent_cfg.normalize();
    if provider.auth == AuthKind::ApiKey && agent_cfg.llama_api_key.trim().is_empty() {
        bail!(
            "Provider {} has no saved API key. Save its key once in the native Provider settings before assigning a subagent to it",
            provider.name
        );
    }
    Ok((agent_cfg, provider_id, model))
}

#[allow(clippy::too_many_arguments)]
async fn tool_agent(
    cfg: &AppConfig,
    paths: &AppPaths,
    client: &LlamaClient,
    current_model: &str,
    args: &Map<String, Value>,
    tool_ctx: &ToolContext,
    runtime_profile: &RuntimeProfile,
    pending_questions: &PendingQuestions,
    pending_approvals: &PendingApprovals,
    runtime: &RuntimeHandles,
    config_state: Option<Arc<RwLock<AppConfig>>>,
    local_web: bool,
    turn: &TurnStream,
) -> anyhow::Result<Value> {
    let prompt = required_string(args, "prompt")?;
    let description = value_string(args, "description").unwrap_or_else(|| "Delegated task".into());
    let subagent_type = value_string(args, "subagent_type")
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "general-purpose".into());
    let agent_profile = AgentProfile::from_subagent_type(&subagent_type);
    let requested_provider = value_string(args, "provider_id").unwrap_or_else(|| "inherit".into());
    let requested_model = value_string(args, "model")
        .filter(|value| !value.trim().is_empty() && !value.eq_ignore_ascii_case("inherit"));
    let (agent_cfg, agent_provider, agent_model) = resolve_agent_provider(
        cfg,
        current_model,
        &requested_provider,
        requested_model.as_deref(),
    )?;
    let isolation = value_string(args, "isolation")
        .unwrap_or_else(|| "local".into())
        .to_lowercase();
    let run_in_background = value_bool(args, "run_in_background").unwrap_or(false);
    let max_depth = cfg.agent_max_depth.max(1);
    if tool_ctx.agent_depth >= max_depth {
        bail!("Agent nesting limit reached ({max_depth})");
    }

    if isolation == "remote" {
        return launch_remote_agent(
            &agent_cfg,
            paths,
            runtime,
            &tool_ctx.session_key,
            &prompt,
            &description,
            &subagent_type,
            &agent_model,
            tool_ctx.agent_id.as_deref(),
            tool_ctx.agent_depth + 1,
            &agent_provider,
        )
        .await;
    }
    if isolation != "local" {
        bail!("isolation must be local or remote");
    }

    let task = tasks::create_runtime_task(
        paths,
        &tool_ctx.session_key,
        "local_agent",
        &description,
        &description,
        &prompt,
        "running",
        json!({
            "agent_type": agent_profile.label(),
            "requested_agent_type": subagent_type.as_str(),
            "provider_id": agent_provider.as_str(),
            "model": agent_model.as_str(),
            "isolation": "local",
            "background": run_in_background,
            "parent_agent_id": tool_ctx.agent_id,
            "agent_depth": tool_ctx.agent_depth + 1,
            "source": source_channel_for_scope(&tool_ctx.session_key),
            "workspace": paths.workspace_dir.to_string_lossy(),
        }),
    )?;
    let task_id = task
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("runtime task was created without an id"))?
        .to_string();
    let output_file = task
        .get("outputFile")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let (cancel_tx, cancel_rx) = oneshot::channel();
    if !runtime
        .register_agent(
            task_id.clone(),
            cancel_tx,
            agent_cfg.agent_max_concurrent.max(1) as usize,
        )
        .await
    {
        let message = format!(
            "Concurrent subagent limit reached ({})",
            agent_cfg.agent_max_concurrent.max(1)
        );
        let _ = tasks::mark_task_terminal(paths, &task_id, "failed", None, Some(&message));
        bail!(message);
    }

    let _ = tasks::append_task_output_text(
        paths,
        &task_id,
        &format!(
            "[{}] {} subagent started: {}\n\n",
            runtime_timestamp(),
            agent_profile.label(),
            description
        ),
    );

    if run_in_background {
        let worker_paths = (*paths).clone();
        let worker_cfg = agent_cfg.clone();
        let worker_client = client.clone();
        let worker_pending = pending_questions.clone();
        let worker_approvals = pending_approvals.clone();
        let worker_runtime = runtime.clone();
        let worker_runtime_profile = runtime_profile.clone();
        let worker_config_state = config_state.clone();
        let worker_task_id = task_id.clone();
        let worker_scope = tool_ctx.session_key.clone();
        let worker_prompt = prompt.clone();
        let worker_description = description.clone();
        let worker_subagent_type = subagent_type.clone();
        let worker_agent_profile = agent_profile;
        let worker_model = agent_model.clone();
        let worker_depth = tool_ctx.agent_depth + 1;
        let worker_memory_block = tool_ctx.memory_block.clone();
        std::thread::spawn(move || {
            let fallback_paths = worker_paths.clone();
            let fallback_task_id = worker_task_id.clone();
            match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt.block_on(run_background_agent(
                    worker_paths,
                    worker_cfg,
                    worker_client,
                    worker_pending,
                    worker_approvals,
                    worker_runtime,
                    worker_runtime_profile,
                    worker_config_state,
                    worker_task_id,
                    worker_scope,
                    worker_prompt,
                    worker_model,
                    worker_description,
                    worker_subagent_type,
                    worker_agent_profile,
                    worker_depth,
                    worker_memory_block,
                    cancel_rx,
                    local_web,
                )),
                Err(err) => {
                    let message = format!("Failed to start local agent runtime: {err}");
                    let _ = tasks::append_task_output_text(
                        &fallback_paths,
                        &fallback_task_id,
                        &format!("[{}] {message}\n", runtime_timestamp()),
                    );
                    let _ = tasks::mark_task_terminal(
                        &fallback_paths,
                        &fallback_task_id,
                        "failed",
                        None,
                        Some(&message),
                    );
                }
            }
        });

        return Ok(json!({
            "status": "async_launched",
            "agentId": task_id.as_str(),
            "taskId": task_id.as_str(),
            "description": description,
            "prompt": prompt,
            "subagentType": agent_profile.label(),
            "providerId": agent_provider,
            "parentAgentId": tool_ctx.agent_id,
            "outputFile": output_file,
        }));
    }

    let system_prompt = build_agent_system_prompt(&description, &subagent_type);
    // A subagent keeps the parent's cancellation but not its event sink: its
    // tokens belong to a separate conversation and would otherwise interleave
    // into the answer the user is watching. Its progress is already visible
    // through the task APIs.
    let subagent_turn = turn.without_sink();
    let response = tokio::select! {
        reply = Box::pin(run_tool_loop_internal(
            client,
            &agent_cfg,
            paths,
            &agent_model,
            &system_prompt,
            runtime_profile,
            tool_ctx.memory_block.as_deref(),
            &prompt,
            Some(&tool_ctx.session_key),
            pending_questions,
            pending_approvals,
            runtime,
            config_state.clone(),
            tool_ctx.agent_depth + 1,
            local_web,
            Some(&task_id),
            agent_profile,
            &subagent_turn,
        )) => Some(reply),
        _ = cancel_rx => None,
    };
    runtime.remove(&task_id).await;
    let Some(result) = response else {
        let message = "Subagent stopped";
        let _ = tasks::append_task_output_text(
            paths,
            &task_id,
            &format!("[{}] {message}\n", runtime_timestamp()),
        );
        let _ = tasks::mark_task_terminal(paths, &task_id, "killed", None, Some(message));
        return Ok(json!({
            "status": "killed",
            "agentId": task_id,
            "taskId": task_id,
            "description": description,
        }));
    };
    let result = result.trim().to_string();
    let _ = tasks::append_task_output_text(paths, &task_id, &(result.clone() + "\n"));
    let _ = tasks::mark_task_terminal(paths, &task_id, "completed", Some(&result), None);
    Ok(json!({
        "status": "completed",
        "agentId": task_id.as_str(),
        "taskId": task_id.as_str(),
        "prompt": prompt,
        "description": description,
        "subagentType": agent_profile.label(),
        "providerId": agent_provider,
        "parentAgentId": tool_ctx.agent_id,
        "outputFile": output_file,
        "result": result,
    }))
}

/// Launch a user-configured background subagent from the shared WebTool
/// registry. It deliberately goes through the same Agent implementation as a
/// model-issued delegation, including provider credentials, sandbox policy,
/// approvals, depth/concurrency limits, and durable task metadata.
#[allow(clippy::too_many_arguments)]
pub async fn launch_background_subagent(
    client: &LlamaClient,
    cfg: &AppConfig,
    paths: &AppPaths,
    current_model: &str,
    runtime_profile: &RuntimeProfile,
    pending_questions: &PendingQuestions,
    pending_approvals: &PendingApprovals,
    runtime: &RuntimeHandles,
    config_state: Option<Arc<RwLock<AppConfig>>>,
    local_web: bool,
    scope_key: &str,
    description: &str,
    prompt: &str,
    subagent_type: &str,
    provider_id: &str,
    model: Option<&str>,
    memory_block: Option<String>,
) -> anyhow::Result<Value> {
    let mut args = Map::new();
    args.insert("description".into(), json!(description));
    args.insert("prompt".into(), json!(prompt));
    args.insert("subagent_type".into(), json!(subagent_type));
    args.insert("provider_id".into(), json!(provider_id));
    args.insert("model".into(), json!(model.unwrap_or("inherit")));
    args.insert("run_in_background".into(), json!(true));
    args.insert("isolation".into(), json!("local"));
    let session_key = normalize_ws(scope_key);
    let tool_ctx = ToolContext {
        approval_scope: approval_scope_for(&session_key, None),
        session_key,
        agent_depth: 0,
        agent_id: None,
        agent_profile: AgentProfile::Root,
        memory_block,
    };
    tool_agent(
        cfg,
        paths,
        client,
        current_model,
        &args,
        &tool_ctx,
        runtime_profile,
        pending_questions,
        pending_approvals,
        runtime,
        config_state,
        local_web,
        &TurnStream::detached(),
    )
    .await
}

async fn tool_read(paths: &AppPaths, args: &Map<String, Value>) -> anyhow::Result<Value> {
    let path = resolve_workspace_path(paths, &required_string(args, "path")?, false)?;
    if !path.exists() {
        bail!("File not found: {}", path.display());
    }
    if path.is_dir() {
        bail!("Path is a directory: {}", path.display());
    }
    let max_chars = value_u64(args, "max_chars")
        .unwrap_or(20_000)
        .clamp(1, 200_000) as usize;
    let text = tokio::fs::read_to_string(&path).await.unwrap_or_else(|_| {
        fs::read(&path)
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default()
    });
    Ok(json!({
        "path": path.to_string_lossy(),
        "content": text.chars().take(max_chars).collect::<String>(),
        "truncated": text.chars().count() > max_chars,
        "size_chars": text.chars().count(),
    }))
}

async fn tool_write(paths: &AppPaths, args: &Map<String, Value>) -> anyhow::Result<Value> {
    let path = resolve_workspace_path(paths, &required_string(args, "path")?, false)?;
    let content = value_string(args, "content").unwrap_or_default();
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(&path, &content).await?;
    Ok(json!({"path": path.to_string_lossy(), "written_chars": content.chars().count()}))
}

async fn tool_edit(paths: &AppPaths, args: &Map<String, Value>) -> anyhow::Result<Value> {
    let path = resolve_workspace_path(paths, &required_string(args, "path")?, false)?;
    let old_text = required_string(args, "old_text")?;
    let new_text = value_string(args, "new_text").unwrap_or_default();
    let replace_all = value_bool(args, "replace_all").unwrap_or(false);
    if !path.exists() {
        bail!("File not found: {}", path.display());
    }
    let current = tokio::fs::read_to_string(&path).await?;
    let count = current.matches(&old_text).count();
    if count == 0 {
        bail!("old_text not found");
    }
    if count > 1 && !replace_all {
        bail!("old_text appears multiple times; set replace_all=true");
    }
    let updated = if replace_all {
        current.replace(&old_text, &new_text)
    } else {
        current.replacen(&old_text, &new_text, 1)
    };
    tokio::fs::write(&path, updated).await?;
    Ok(json!({"path": path.to_string_lossy(), "replacements": if replace_all { count } else { 1 }}))
}

fn tool_glob(paths: &AppPaths, args: &Map<String, Value>) -> anyhow::Result<Value> {
    let pattern = required_string(args, "pattern")?;
    let base_path = resolve_workspace_path(
        paths,
        &value_string(args, "path").unwrap_or_else(|| ".".into()),
        false,
    )?;
    let limit = value_u64(args, "limit").unwrap_or(200).clamp(1, 1000) as usize;
    let matcher = wildcard_regex(&pattern)?;
    let mut matches = Vec::new();
    collect_glob_matches(
        &base_path,
        &base_path,
        &matcher,
        &pattern,
        limit + 1,
        &mut matches,
    )?;
    Ok(json!({
        "pattern": pattern,
        "base_path": base_path.to_string_lossy(),
        "matches": matches.iter().take(limit).collect::<Vec<_>>(),
        "truncated": matches.len() > limit,
    }))
}

async fn tool_grep(paths: &AppPaths, args: &Map<String, Value>) -> anyhow::Result<Value> {
    let pattern = required_string(args, "pattern")?;
    let base_path = resolve_workspace_path(
        paths,
        &value_string(args, "path").unwrap_or_else(|| ".".into()),
        false,
    )?;
    let limit = value_u64(args, "limit")
        .unwrap_or(200)
        .clamp(1, 1000)
        .to_string();
    let output = timeout(
        Duration::from_secs(20),
        Command::new("rg")
            .arg("-n")
            .arg("--no-heading")
            .arg("--color")
            .arg("never")
            .arg("-m")
            .arg(&limit)
            .arg(&pattern)
            .arg(&base_path)
            .output(),
    )
    .await
    .context("rg timed out")??;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let matches = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    Ok(json!({
        "pattern": pattern,
        "path": base_path.to_string_lossy(),
        "matches": matches,
        "exit_code": output.status.code().unwrap_or(-1),
    }))
}

async fn authorize_standard(
    cfg: &AppConfig,
    pending_approvals: &PendingApprovals,
    scope_key: &str,
    tool: &str,
    command: &str,
    reason: &str,
    paths: &AppPaths,
) -> anyhow::Result<()> {
    match web_sandbox_mode(cfg) {
        SandboxMode::ReadOnly => bail!("{tool} is disabled in read-only mode"),
        SandboxMode::FullAccess => Ok(()),
        SandboxMode::Normal | SandboxMode::IsolatedWorkspaceWrite
            if is_whatsapp_scope(scope_key) =>
        {
            Ok(())
        }
        SandboxMode::Normal | SandboxMode::IsolatedWorkspaceWrite => {
            let approved = pending_approvals
                .request_standard(
                    scope_key,
                    tool,
                    command,
                    reason,
                    &paths.workspace_dir.to_string_lossy(),
                )
                .await?;
            if approved {
                Ok(())
            } else {
                bail!("{tool} was denied by the user")
            }
        }
    }
}

fn web_sandbox_mode(cfg: &AppConfig) -> SandboxMode {
    match cfg.web_sandbox_mode.as_str() {
        "read-only" => SandboxMode::ReadOnly,
        "full-access" => SandboxMode::FullAccess,
        _ => SandboxMode::Normal,
    }
}

fn web_sandbox_policy(cfg: &AppConfig, cwd: PathBuf, timeout_seconds: u64) -> SandboxPolicy {
    let mut policy = match web_sandbox_mode(cfg) {
        SandboxMode::ReadOnly => SandboxPolicy::read_only(cwd),
        SandboxMode::FullAccess => SandboxPolicy::full_access(cwd),
        SandboxMode::Normal | SandboxMode::IsolatedWorkspaceWrite => SandboxPolicy::normal(cwd),
    };
    policy.timeout_ms = timeout_seconds.saturating_mul(1_000);
    policy.max_output_bytes = 256 * 1024;
    policy
}

#[allow(clippy::too_many_arguments)]
async fn tool_bash(
    cfg: &AppConfig,
    paths: &AppPaths,
    args: &Map<String, Value>,
    runtime: &RuntimeHandles,
    scope_key: &str,
    approval_scope: &str,
    pending_approvals: &PendingApprovals,
    turn: &TurnStream,
) -> anyhow::Result<Value> {
    let command = required_string(args, "command")?;
    validate_bash_command(&command)?;
    if command_requests_privilege(&command) {
        bail!("Bash cannot invoke sudo; use the dedicated Sudo tool")
    }
    let cwd = resolve_workspace_path(
        paths,
        &value_string(args, "cwd").unwrap_or_else(|| ".".into()),
        false,
    )?;
    if !cwd.is_dir() {
        bail!("cwd is not a directory: {}", cwd.display());
    }
    authorize_standard(
        cfg,
        pending_approvals,
        approval_scope,
        "Bash",
        &command,
        "The command runs with your normal OS account and may modify files or start processes.",
        paths,
    )
    .await?;
    let secs = value_u64(args, "timeout").unwrap_or(20).clamp(1, 120);
    let policy = web_sandbox_policy(cfg, cwd.clone(), secs);
    if value_bool(args, "run_in_background").unwrap_or(false) {
        return launch_background_bash(paths, runtime, scope_key, command, cwd, policy).await;
    }
    let output = spawn_sandboxed_with_cancel(
        &policy,
        "bash",
        &["-lc".into(), command.clone()],
        turn.cancel_token(),
    )
    .await?;
    if output.cancelled {
        bail!("Bash was interrupted by the user")
    }
    Ok(json!({
        "command": command,
        "cwd": cwd.to_string_lossy(),
        "exit_code": output.exit_code.unwrap_or(-1),
        "stdout": tail(&output.stdout, 12_000),
        "stderr": tail(&output.stderr, 12_000),
        "timed_out": output.timed_out,
        "truncated": output.truncated,
    }))
}

#[allow(clippy::too_many_arguments)]
async fn tool_sudo(
    cfg: &AppConfig,
    paths: &AppPaths,
    args: &Map<String, Value>,
    scope_key: &str,
    pending_approvals: &PendingApprovals,
    local_web: bool,
    turn: &TurnStream,
) -> anyhow::Result<Value> {
    if !local_web {
        bail!("Sudo is disabled when the background service listens on a non-loopback address")
    }
    if web_sandbox_mode(cfg) == SandboxMode::ReadOnly {
        bail!("Sudo is disabled in read-only mode")
    }
    let command = required_string(args, "command")?;
    let cwd = resolve_workspace_path(
        paths,
        &value_string(args, "cwd").unwrap_or_else(|| ".".into()),
        false,
    )?;
    if !cwd.is_dir() {
        bail!("cwd is not a directory: {}", cwd.display());
    }
    let whatsapp_origin = is_whatsapp_scope(scope_key);
    if !whatsapp_origin {
        let approved = pending_approvals
            .request_standard(
                scope_key,
                "Sudo",
                &command,
                "This command will run as root. Full-access never bypasses this confirmation.",
                &paths.workspace_dir.to_string_lossy(),
            )
            .await?;
        if !approved {
            bail!("Sudo was denied by the user")
        }
    }

    let cancel = turn.cancel_token().clone();
    ensure_web_sudo_authenticated(
        pending_approvals,
        scope_key,
        &command,
        &cancel,
        !whatsapp_origin,
    )
    .await?;
    let secs = value_u64(args, "timeout").unwrap_or(60).clamp(1, 600);
    let mut policy = SandboxPolicy::full_access(cwd.clone());
    policy.allow_privilege_escalation = true;
    policy.timeout_ms = secs.saturating_mul(1_000);
    policy.max_output_bytes = 256 * 1024;
    let output = spawn_sandboxed_with_cancel(
        &policy,
        "sudo",
        &[
            "-n".into(),
            "--".into(),
            "bash".into(),
            "-lc".into(),
            command.clone(),
        ],
        &cancel,
    )
    .await?;
    Ok(json!({
        "command": command,
        "cwd": cwd.to_string_lossy(),
        "exit_code": output.exit_code.unwrap_or(-1),
        "stdout": tail(&output.stdout, 12_000),
        "stderr": tail(&output.stderr, 12_000),
        "timed_out": output.timed_out,
        "truncated": output.truncated,
    }))
}

async fn ensure_web_sudo_authenticated(
    pending_approvals: &PendingApprovals,
    scope_key: &str,
    command: &str,
    cancel: &CancellationToken,
    allow_interactive_prompt: bool,
) -> anyhow::Result<()> {
    if validate_sudo(None, cancel).await? {
        return Ok(());
    }
    let has_keyring = keyring_available();
    if has_keyring {
        match lookup_keyring_secret(cancel).await {
            Ok(Some(secret)) if validate_sudo(Some(&secret), cancel).await? => return Ok(()),
            Ok(Some(_)) => clear_keyring_secret().await,
            Ok(None) => {}
            Err(error) => warn!("desktop keyring lookup failed: {error}"),
        }
    }

    if !allow_interactive_prompt {
        bail!(
            "Sudo from WhatsApp requires an active sudo ticket or a valid credential already saved in the desktop keyring; authenticate locally once and enable keyring storage"
        )
    }

    let mut message = None;
    for attempt in 1..=3 {
        let Some((secret, remember)) = pending_approvals
            .request_credential(scope_key, command, has_keyring, attempt, message.take())
            .await?
        else {
            bail!("sudo authentication was cancelled by the user")
        };
        if validate_sudo(Some(&secret), cancel).await? {
            if remember && has_keyring {
                if let Err(error) = store_keyring_secret(&secret, cancel).await {
                    warn!("sudo authenticated, but the keyring did not save it: {error}");
                }
            }
            return Ok(());
        }
        message = Some("The credential was not accepted by sudo. Try again.".into());
    }
    bail!("sudo authentication failed after three attempts")
}

async fn launch_background_bash(
    paths: &AppPaths,
    runtime: &RuntimeHandles,
    scope_key: &str,
    command: String,
    cwd: PathBuf,
    policy: SandboxPolicy,
) -> anyhow::Result<Value> {
    let summary = short_command(&command, 80);
    let task = tasks::create_runtime_task(
        paths,
        scope_key,
        "local_bash",
        &format!("Bash: {summary}"),
        &format!("Background Bash command: {summary}"),
        &command,
        "running",
        json!({"command": command.as_str(), "cwd": cwd.to_string_lossy()}),
    )?;
    let task_id = task
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("runtime task was created without an id"))?
        .to_string();
    let output_file = task
        .get("outputFile")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let (cancel_tx, cancel_rx) = oneshot::channel();
    runtime.register(task_id.clone(), cancel_tx).await;

    let worker_paths = (*paths).clone();
    let worker_runtime = runtime.clone();
    let worker_task_id = task_id.clone();
    let worker_command = command.clone();
    let worker_cwd = cwd.clone();
    tokio::spawn(async move {
        run_background_bash(
            worker_paths,
            worker_runtime,
            worker_task_id,
            worker_command,
            worker_cwd,
            policy,
            cancel_rx,
        )
        .await;
    });

    Ok(json!({
        "status": "async_launched",
        "taskId": task_id.as_str(),
        "backgroundTaskId": task_id.as_str(),
        "shell_id": task_id.as_str(),
        "outputFile": output_file,
        "command": command.as_str(),
        "cwd": cwd.to_string_lossy(),
    }))
}

async fn run_background_bash(
    paths: AppPaths,
    runtime: RuntimeHandles,
    task_id: String,
    command: String,
    cwd: PathBuf,
    policy: SandboxPolicy,
    mut cancel_rx: oneshot::Receiver<()>,
) {
    let _ = tasks::append_task_output_text(
        &paths,
        &task_id,
        &format!(
            "[{}] Bash started\ncwd: {}\ncommand: {}\n\n",
            runtime_timestamp(),
            cwd.display(),
            command
        ),
    );

    let mut bash = match sandboxed_command(&policy, "bash", &["-lc".into(), command.clone()]) {
        Ok(command) => command,
        Err(err) => {
            let error = format!("Failed to prepare sandboxed Bash command: {err}");
            let _ = tasks::mark_task_terminal(&paths, &task_id, "failed", None, Some(&error));
            runtime.remove(&task_id).await;
            return;
        }
    };
    bash.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = match bash.spawn() {
        Ok(child) => child,
        Err(err) => {
            let error = format!("Failed to spawn Bash command: {err}");
            warn!("{error}");
            let _ = tasks::append_task_output_text(
                &paths,
                &task_id,
                &format!("[{}] {error}\n", runtime_timestamp()),
            );
            let _ = tasks::mark_task_terminal(&paths, &task_id, "failed", None, Some(&error));
            runtime.remove(&task_id).await;
            return;
        }
    };

    let stdout_reader = child.stdout.take().map(|mut pipe| {
        tokio::spawn(async move {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes).await;
            String::from_utf8_lossy(&bytes).into_owned()
        })
    });
    let stderr_reader = child.stderr.take().map(|mut pipe| {
        tokio::spawn(async move {
            let mut bytes = Vec::new();
            let _ = pipe.read_to_end(&mut bytes).await;
            String::from_utf8_lossy(&bytes).into_owned()
        })
    });

    let (cancelled, wait_result) = tokio::select! {
        status = child.wait() => (false, status),
        _ = &mut cancel_rx => {
            terminate_child_process_group(&mut child).await;
            (true, child.wait().await)
        }
    };

    let stdout = join_pipe_reader(stdout_reader).await;
    let stderr = join_pipe_reader(stderr_reader).await;
    if !stdout.is_empty() {
        let _ = tasks::append_task_output_text(
            &paths,
            &task_id,
            &format!("[stdout]\n{}\n", tail(&stdout, 200_000)),
        );
    }
    if !stderr.is_empty() {
        let _ = tasks::append_task_output_text(
            &paths,
            &task_id,
            &format!("[stderr]\n{}\n", tail(&stderr, 200_000)),
        );
    }

    match wait_result {
        Ok(status) if cancelled => {
            let message = format!(
                "Command stopped{}",
                status
                    .code()
                    .map(|code| format!(" with exit code {code}"))
                    .unwrap_or_default()
            );
            let _ = tasks::append_task_output_text(
                &paths,
                &task_id,
                &format!("[{}] {message}\n", runtime_timestamp()),
            );
            let _ = tasks::mark_task_terminal(&paths, &task_id, "killed", None, Some(&message));
        }
        Ok(status) if status.success() => {
            let result = format!("Exit code: {}", status.code().unwrap_or(0));
            let _ = tasks::append_task_output_text(
                &paths,
                &task_id,
                &format!("[{}] Bash completed ({result})\n", runtime_timestamp()),
            );
            let _ = tasks::mark_task_terminal(&paths, &task_id, "completed", Some(&result), None);
        }
        Ok(status) => {
            let error = format!("Exit code: {}", status.code().unwrap_or(-1));
            let _ = tasks::append_task_output_text(
                &paths,
                &task_id,
                &format!("[{}] Bash failed ({error})\n", runtime_timestamp()),
            );
            let _ = tasks::mark_task_terminal(&paths, &task_id, "failed", None, Some(&error));
        }
        Err(err) => {
            let error = format!("Failed to wait for Bash command: {err}");
            warn!("{error}");
            let _ = tasks::append_task_output_text(
                &paths,
                &task_id,
                &format!("[{}] {error}\n", runtime_timestamp()),
            );
            let _ = tasks::mark_task_terminal(&paths, &task_id, "failed", None, Some(&error));
        }
    }
    runtime.remove(&task_id).await;
}

async fn join_pipe_reader(reader: Option<tokio::task::JoinHandle<String>>) -> String {
    match reader {
        Some(reader) => reader.await.unwrap_or_default(),
        None => String::new(),
    }
}

async fn terminate_child_process_group(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGTERM);
        }
        sleep(Duration::from_millis(500)).await;
        if child.try_wait().ok().flatten().is_none() {
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
        return;
    }
    let _ = child.start_kill();
}

async fn tool_config(
    cfg: &AppConfig,
    paths: &AppPaths,
    args: &Map<String, Value>,
    config_state: Option<Arc<RwLock<AppConfig>>>,
) -> anyhow::Result<Value> {
    let setting = required_string(args, "setting")?;
    let descriptions = config_tool_settings();
    let description = descriptions
        .iter()
        .find(|(key, _)| *key == setting)
        .map(|(_, description)| *description)
        .ok_or_else(|| {
            anyhow!(
                "Unknown setting. Supported settings: {}",
                descriptions
                    .iter()
                    .map(|(key, _)| *key)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;

    let current = serde_json::to_value(cfg)?;
    let previous_value = current.get(&setting).cloned().unwrap_or(Value::Null);
    if !args.contains_key("value") {
        return Ok(json!({
            "success": true,
            "operation": "get",
            "setting": setting,
            "value": previous_value,
            "description": description,
        }));
    }

    let value = coerce_config_value(
        &setting,
        args.get("value").unwrap_or(&Value::Null),
        &previous_value,
    )?;
    if setting == "llama_api_mode" && !matches!(value.as_str(), Some("chat") | Some("completion")) {
        bail!("llama_api_mode must be 'chat' or 'completion'");
    }
    let mut updated = current
        .as_object()
        .cloned()
        .ok_or_else(|| anyhow!("config must serialize to an object"))?;
    updated.insert(setting.clone(), value.clone());
    let mut new_cfg: AppConfig = serde_json::from_value(Value::Object(updated))?;
    if setting == "llama_api_key" {
        let provider_id = new_cfg.provider_id.clone();
        new_cfg.remember_provider_api_key(&provider_id, value.as_str());
    }
    new_cfg.normalize();
    new_cfg.save(&paths.config_file)?;
    if let Some(config_state) = config_state {
        *config_state.write().await = new_cfg;
    }

    Ok(json!({
        "success": true,
        "operation": "set",
        "setting": setting,
        "previousValue": previous_value,
        "newValue": value,
        "description": description,
        "note": "Saved to config.json and reloaded into active server state when available.",
    }))
}

async fn tool_node(
    cfg: &AppConfig,
    args: &Map<String, Value>,
    centrally_approved: bool,
) -> anyhow::Result<Value> {
    if !cfg.node_hub_enabled {
        bail!("remote nodes are disabled; enable the Node Hub in Settings and restart GnomeAI")
    }
    let client = nodes::local_client(cfg.node_hub_port, &cfg.node_hub_admin_token);
    let action = value_string(args, "action").unwrap_or_else(|| "list".into());
    match action.as_str() {
        "list" => client.list().await,
        "exec" => {
            let node_id = required_string(args, "node_id")?;
            let command = required_string(args, "command")?;
            let timeout_secs = value_u64(args, "timeout").unwrap_or(60).clamp(1, 3_600);
            client
                .execute(
                    &node_id,
                    &QueueJobRequest {
                        action: "shell".into(),
                        command,
                        stdin: String::new(),
                        cwd: value_string(args, "cwd"),
                        timeout_secs,
                        root: value_bool(args, "root").unwrap_or(false),
                        root_approved: centrally_approved,
                    },
                )
                .await
        }
        other => bail!("unknown Node action `{other}`; use `list` or `exec`"),
    }
}

fn tool_skill(paths: &AppPaths, args: &Map<String, Value>) -> anyhow::Result<Value> {
    let requested_name = value_string(args, "name")
        .map(|item| normalize_ws(&item))
        .unwrap_or_default();
    let query = value_string(args, "query")
        .map(|item| normalize_ws(&item))
        .unwrap_or_else(|| requested_name.clone());
    let resource = value_string(args, "resource")
        .map(|item| normalize_ws(&item))
        .unwrap_or_default();
    let include_content = value_bool(args, "include_content").unwrap_or(true);
    let max_results = value_u64(args, "max_results").unwrap_or(5).clamp(1, 20) as usize;

    if !resource.is_empty() {
        if requested_name.is_empty() {
            bail!("name is required when reading a skill resource");
        }
        let content = skills::read_resource(&paths.workspace_dir, &requested_name, &resource)?;
        let truncated = content.chars().count() > 64_000;
        return Ok(json!({
            "name": requested_name,
            "resource": resource,
            "content": content.chars().take(64_000).collect::<String>(),
            "truncated": truncated,
        }));
    }

    let installed = skills::discover(&paths.workspace_dir);
    if installed.is_empty() {
        return Ok(json!({"matches": [], "query": query, "total_skills": 0}));
    }

    let wanted = query
        .strip_prefix("select:")
        .map(str::trim)
        .unwrap_or(&query)
        .to_ascii_lowercase();
    let selected = if wanted.is_empty() {
        installed
            .iter()
            .take(max_results)
            .cloned()
            .collect::<Vec<_>>()
    } else {
        let terms = wanted
            .split_whitespace()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let mut scored = Vec::new();
        for item in &installed {
            let name = item.name.to_ascii_lowercase();
            let haystack = format!(
                "{} {} {} {}",
                item.name,
                item.description,
                item.scope,
                item.path.to_string_lossy()
            )
            .to_ascii_lowercase();
            let mut score = 0;
            if name == wanted {
                score += 1_000;
            }
            for term in &terms {
                if name.contains(term) {
                    score += 40;
                }
                if haystack.contains(term) {
                    score += 8;
                }
            }
            if score > 0 {
                scored.push((score, item.clone()));
            }
        }
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
        scored
            .into_iter()
            .take(max_results)
            .map(|(_, item)| item)
            .collect::<Vec<_>>()
    };

    let mut matches = Vec::new();
    for item in selected {
        let mut entry = serde_json::to_value(&item)?;
        if include_content {
            let skill = skills::load(&paths.workspace_dir, &item.name)?;
            let content = skills::render_for_model(&skill);
            entry["content"] = json!(content.chars().take(64_000).collect::<String>());
            entry["truncated"] = json!(content.chars().count() > 64_000);
        }
        matches.push(entry);
    }
    Ok(json!({
        "matches": matches,
        "query": query,
        "total_skills": installed.len()
    }))
}

async fn tool_learn(args: &Map<String, Value>) -> anyhow::Result<Value> {
    let platforms = args
        .get("platforms")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let spec = skills::LearnedSkillSpec {
        name: required_string(args, "name")?,
        description: required_string(args, "description")?,
        instructions: required_string(args, "instructions")?,
        script: value_string(args, "script"),
        platforms,
        replace: value_bool(args, "replace").unwrap_or(false),
    };
    let summary = tokio::task::spawn_blocking(move || skills::learn(spec)).await??;
    Ok(json!({"ok": true, "skill": summary}))
}

#[allow(clippy::too_many_arguments)]
async fn tool_run_skill(
    cfg: &AppConfig,
    paths: &AppPaths,
    args: &Map<String, Value>,
    runtime: &RuntimeHandles,
    tool_ctx: &ToolContext,
    pending_approvals: &PendingApprovals,
    turn: &TurnStream,
) -> anyhow::Result<Value> {
    let name = required_string(args, "name")?;
    let entrypoint = skills::entrypoint(&paths.workspace_dir, &name)?;
    let target = value_string(args, "target").unwrap_or_else(|| "local".into());
    let timeout = value_u64(args, "timeout").unwrap_or(120).clamp(1, 120);
    match target.as_str() {
        "local" => {
            if value_bool(args, "root").unwrap_or(false) {
                bail!("local RunSkill root requires the dedicated Sudo tool")
            }
            let mut bash_args = Map::new();
            bash_args.insert(
                "command".into(),
                json!(format!(
                    "exec /bin/sh {}",
                    shell_single_quote(&entrypoint.path.to_string_lossy())
                )),
            );
            bash_args.insert(
                "cwd".into(),
                json!(value_string(args, "cwd").unwrap_or_else(|| ".".into())),
            );
            bash_args.insert("timeout".into(), json!(timeout));
            tool_bash(
                cfg,
                paths,
                &bash_args,
                runtime,
                &tool_ctx.session_key,
                &tool_ctx.approval_scope,
                pending_approvals,
                turn,
            )
            .await
        }
        "node" => {
            if !cfg.node_hub_enabled {
                bail!("remote nodes are disabled")
            }
            let node_id = required_string(args, "node_id")?;
            authorize_standard(
                cfg,
                pending_approvals,
                &tool_ctx.approval_scope,
                "RunSkill",
                &format!("run skill {name} on node {node_id}"),
                "The skill entrypoint will execute on the selected paired device.",
                paths,
            )
            .await?;
            let root_approved = matches!(
                web_sandbox_mode(cfg),
                SandboxMode::Normal | SandboxMode::IsolatedWorkspaceWrite
            ) && !is_whatsapp_scope(&tool_ctx.approval_scope);
            nodes::local_client(cfg.node_hub_port, &cfg.node_hub_admin_token)
                .execute(
                    &node_id,
                    &QueueJobRequest {
                        action: "script".into(),
                        command: String::new(),
                        stdin: entrypoint.script,
                        cwd: value_string(args, "cwd"),
                        timeout_secs: timeout,
                        root: value_bool(args, "root").unwrap_or(false),
                        root_approved,
                    },
                )
                .await
        }
        other => bail!("unknown skill target `{other}`"),
    }
}

fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

async fn tool_web_search(cfg: &AppConfig, args: &Map<String, Value>) -> anyhow::Result<Value> {
    let query = required_string(args, "query")?;
    let allowed = domain_list(args.get("allowed_domains"));
    let blocked = domain_list(args.get("blocked_domains"));
    let mut parts = vec![query];
    if allowed.len() == 1 {
        parts.push(format!("site:{}", allowed[0]));
    } else if allowed.len() > 1 {
        parts.push(format!(
            "({})",
            allowed
                .iter()
                .map(|domain| format!("site:{domain}"))
                .collect::<Vec<_>>()
                .join(" OR ")
        ));
    }
    if !blocked.is_empty() {
        parts.push(
            blocked
                .iter()
                .map(|domain| format!("-site:{domain}"))
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    let final_query = parts.join(" ");
    let result = firecrawl_search(cfg, &final_query).await;
    Ok(json!({"query": final_query, "results": result.entries, "text": result.text}))
}

async fn tool_ask_user_question(
    pending_questions: &PendingQuestions,
    scope_key: &str,
    args: &Map<String, Value>,
) -> anyhow::Result<Value> {
    let questions = args
        .get("questions")
        .ok_or_else(|| anyhow!("questions is required"))?;
    let timeout_seconds = value_u64(args, "timeout_seconds").unwrap_or(3600);
    pending_questions
        .ask(scope_key, questions, timeout_seconds)
        .await
        .map_err(anyhow::Error::from)
}

async fn tool_task_output(
    paths: &AppPaths,
    scope_key: &str,
    args: &Map<String, Value>,
) -> anyhow::Result<Value> {
    let task_id = value_string(args, "task_id")
        .or_else(|| value_string(args, "taskId"))
        .ok_or_else(|| anyhow!("task_id is required"))?;
    let block = value_bool(args, "block").unwrap_or(true);
    let timeout_ms = value_u64(args, "timeout").unwrap_or(30_000).min(600_000);
    if !block {
        return tasks::task_output(paths, scope_key, &task_id, false);
    }

    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
    let mut latest = tasks::task_output(paths, scope_key, &task_id, false)?;
    while tokio::time::Instant::now() < deadline {
        let status = latest
            .get("retrieval_status")
            .and_then(Value::as_str)
            .unwrap_or("");
        if status == "success" || status == "not_found" {
            return Ok(latest);
        }
        sleep(Duration::from_millis(200)).await;
        latest = tasks::task_output(paths, scope_key, &task_id, false)?;
    }
    latest["retrieval_status"] = json!("timeout");
    Ok(latest)
}

async fn tool_web_fetch(
    cfg: &AppConfig,
    client: &LlamaClient,
    model: &str,
    args: &Map<String, Value>,
) -> anyhow::Result<Value> {
    let url = required_string(args, "url")?;
    let prompt = value_string(args, "prompt").unwrap_or_default();
    let result = firecrawl_fetch(cfg, &url).await;
    let mut value = serde_json::to_value(&result)?;
    if !prompt.trim().is_empty() && !result.content.trim().is_empty() {
        let response = client
            .chat(
                cfg,
                model,
                vec![
                    json!({
                        "role": "system",
                        "content": "Use only the provided page content. Answer briefly and quote concrete details only when they appear in the content."
                    }),
                    json!({
                        "role": "user",
                        "content": format!("URL: {url}\n\nContent:\n{}\n\nTask: {prompt}", result.content)
                    }),
                ],
                0.1,
            )
            .await;
        value["prompt_result"] = json!(
            response
                .map(|item| item.content)
                .unwrap_or_else(|err| { format!("[LLM error: {err}]") })
        );
    }
    Ok(value)
}

async fn launch_remote_agent(
    cfg: &AppConfig,
    paths: &AppPaths,
    runtime: &RuntimeHandles,
    scope_key: &str,
    prompt: &str,
    description: &str,
    subagent_type: &str,
    model: &str,
    parent_agent_id: Option<&str>,
    agent_depth: u32,
    provider_id: &str,
) -> anyhow::Result<Value> {
    let launcher = cfg.remote_agent_api_url.trim().trim_end_matches('/');
    if launcher.is_empty() {
        bail!("Remote agent launcher is not configured. Set remote_agent_api_url first.");
    }
    let task = tasks::create_runtime_task(
        paths,
        scope_key,
        "remote_agent",
        description,
        description,
        prompt,
        "pending",
        json!({
            "agent_type": AgentProfile::from_subagent_type(subagent_type).label(),
            "requested_agent_type": subagent_type,
            "provider_id": provider_id,
            "model": model,
            "isolation": "remote",
            "background": true,
            "parent_agent_id": parent_agent_id,
            "agent_depth": agent_depth,
            "source": source_channel_for_scope(scope_key),
            "workspace": paths.workspace_dir.to_string_lossy(),
        }),
    )?;
    let task_id = task
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("runtime task was created without an id"))?
        .to_string();
    let output_file = task
        .get("outputFile")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let launch_url = format!("{launcher}/launch");
    let payload = json!({
        "task_id": task_id.as_str(),
        "session_key": scope_key,
        "description": description,
        "prompt": prompt,
        "model": model,
        "provider_id": provider_id,
        "subagent_type": subagent_type,
        "parent_agent_id": parent_agent_id,
        "agent_depth": agent_depth,
        "callback_url": format!(
            "http://{}:{}/api/runtime/tasks/{}/update?token={}",
            cfg.host, cfg.port, task_id, cfg.web_api_token
        ),
    });

    let http = reqwest::Client::new();
    let data = match remote_agent_request(http.post(&launch_url), cfg)
        .json(&payload)
        .send()
        .await
    {
        Ok(response) => match read_json_response(response).await {
            Ok(data) => data,
            Err(err) => {
                let message = err.to_string();
                let _ = tasks::mark_task_terminal(paths, &task_id, "failed", None, Some(&message));
                bail!("Remote agent launch failed: {message}");
            }
        },
        Err(err) => {
            let message = err.to_string();
            let _ = tasks::mark_task_terminal(paths, &task_id, "failed", None, Some(&message));
            bail!("Remote agent launch failed: {message}");
        }
    };

    let session_url = data
        .get("session_url")
        .and_then(Value::as_str)
        .map(normalize_ws)
        .unwrap_or_default();
    let poll_url = data
        .get("poll_url")
        .and_then(Value::as_str)
        .map(normalize_ws)
        .unwrap_or_default();
    let _ = tasks::runtime_task_update(
        paths,
        &task_id,
        &json!({
            "status": "running",
            "session_url": session_url,
            "message": "Remote agent running",
        }),
    );
    if !poll_url.is_empty() {
        let mut update = Map::new();
        update.insert("taskId".into(), json!(task_id.as_str()));
        update.insert("metadata".into(), json!({"poll_url": poll_url.as_str()}));
        let _ = tasks::task_update(paths, scope_key, &update);

        let (cancel_tx, cancel_rx) = oneshot::channel();
        runtime.register(task_id.clone(), cancel_tx).await;
        let worker_paths = (*paths).clone();
        let worker_cfg = cfg.clone();
        let worker_runtime = runtime.clone();
        let worker_task_id = task_id.clone();
        tokio::spawn(async move {
            poll_remote_agent_task(
                worker_paths,
                worker_cfg,
                worker_runtime,
                worker_task_id,
                poll_url,
                cancel_rx,
            )
            .await;
        });
    }

    Ok(json!({
        "status": "remote_launched",
        "taskId": task_id.as_str(),
        "agentId": task_id.as_str(),
        "description": description,
        "prompt": prompt,
        "sessionUrl": session_url,
        "outputFile": output_file,
    }))
}

async fn read_json_response(response: reqwest::Response) -> anyhow::Result<Value> {
    let status = response.status();
    let response_body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!(
            "HTTP {status}: {}",
            response_body.chars().take(800).collect::<String>()
        );
    }
    if response_body.trim().is_empty() {
        Ok(json!({}))
    } else {
        serde_json::from_str(&response_body).context("failed to parse JSON response")
    }
}

async fn poll_remote_agent_task(
    paths: AppPaths,
    cfg: AppConfig,
    runtime: RuntimeHandles,
    task_id: String,
    poll_url: String,
    mut cancel_rx: oneshot::Receiver<()>,
) {
    let http = reqwest::Client::new();
    loop {
        if task_is_terminal(&paths, &task_id) {
            break;
        }

        match remote_agent_request(http.get(&poll_url), &cfg).send().await {
            Ok(response) => match read_json_response(response).await {
                Ok(payload) => {
                    apply_remote_agent_update(&paths, &task_id, &payload);
                    let status = payload
                        .get("status")
                        .and_then(Value::as_str)
                        .map(normalize_ws)
                        .unwrap_or_default();
                    if is_terminal_status(&status) {
                        let result = payload.get("result").and_then(Value::as_str);
                        let error = payload.get("error").and_then(Value::as_str);
                        let _ = tasks::mark_task_terminal(&paths, &task_id, &status, result, error);
                        break;
                    }
                }
                Err(err) => {
                    let _ = tasks::append_task_output_text(
                        &paths,
                        &task_id,
                        &format!("[{}] Remote poll error: {err}\n", runtime_timestamp()),
                    );
                }
            },
            Err(err) => {
                let _ = tasks::append_task_output_text(
                    &paths,
                    &task_id,
                    &format!("[{}] Remote poll error: {err}\n", runtime_timestamp()),
                );
            }
        }

        let cancelled = tokio::select! {
            _ = sleep(Duration::from_secs(5)) => false,
            _ = &mut cancel_rx => true,
        };
        if cancelled {
            let _ = tasks::append_task_output_text(
                &paths,
                &task_id,
                &format!("[{}] Remote agent polling stopped\n", runtime_timestamp()),
            );
            let _ = tasks::mark_task_terminal(
                &paths,
                &task_id,
                "killed",
                None,
                Some("Remote agent polling was stopped"),
            );
            break;
        }
    }
    runtime.remove(&task_id).await;
}

fn apply_remote_agent_update(paths: &AppPaths, task_id: &str, payload: &Value) {
    let Some(obj) = payload.as_object() else {
        return;
    };
    let mut update = Map::new();
    for key in ["append_output", "output", "session_url", "message"] {
        if let Some(value) = obj.get(key).filter(|value| !value.is_null()) {
            update.insert(key.into(), value.clone());
        }
    }
    if let Some(status) = obj
        .get("status")
        .and_then(Value::as_str)
        .map(normalize_ws)
        .filter(|status| !is_terminal_status(status))
    {
        update.insert("status".into(), json!(status));
    }
    if !update.is_empty() {
        let _ = tasks::runtime_task_update(paths, task_id, &Value::Object(update));
    }
}

async fn run_background_agent(
    paths: AppPaths,
    cfg: AppConfig,
    client: LlamaClient,
    pending_questions: PendingQuestions,
    pending_approvals: PendingApprovals,
    runtime: RuntimeHandles,
    runtime_profile: RuntimeProfile,
    config_state: Option<Arc<RwLock<AppConfig>>>,
    task_id: String,
    scope_key: String,
    prompt: String,
    model: String,
    description: String,
    subagent_type: String,
    agent_profile: AgentProfile,
    agent_depth: u32,
    memory_block: Option<String>,
    mut cancel_rx: oneshot::Receiver<()>,
    local_web: bool,
) {
    let system_prompt = build_agent_system_prompt(&description, &subagent_type);
    // A background agent outlives the request that started it, so it has no
    // browser stream to report into; `cancel_rx` below is its stop signal.
    let background_turn = TurnStream::detached();
    let response = tokio::select! {
        reply = Box::pin(run_tool_loop_internal(
            &client,
            &cfg,
            &paths,
            &model,
            &system_prompt,
            &runtime_profile,
            memory_block.as_deref(),
            &prompt,
            Some(&scope_key),
            &pending_questions,
            &pending_approvals,
            &runtime,
            config_state.clone(),
            agent_depth,
            local_web,
            Some(&task_id),
            agent_profile,
            &background_turn,
        )) => Some(reply),
        _ = &mut cancel_rx => None,
    };

    match response {
        Some(result) => {
            let result = result.trim().to_string();
            let _ = tasks::append_task_output_text(&paths, &task_id, &(result.clone() + "\n"));
            let _ = tasks::mark_task_terminal(&paths, &task_id, "completed", Some(&result), None);
        }
        None => {
            let message = "Agent task was stopped";
            let _ = tasks::append_task_output_text(
                &paths,
                &task_id,
                &format!("[{}] {message}\n", runtime_timestamp()),
            );
            let _ = tasks::mark_task_terminal(&paths, &task_id, "killed", None, Some(message));
        }
    }
    runtime.remove(&task_id).await;
}

fn remote_agent_request(
    builder: reqwest::RequestBuilder,
    cfg: &AppConfig,
) -> reqwest::RequestBuilder {
    let builder = builder
        .header("Accept", "application/json")
        .header("Content-Type", "application/json");
    let api_key = cfg.remote_agent_api_key.trim();
    if api_key.is_empty() {
        builder
    } else {
        builder.bearer_auth(api_key)
    }
}

fn build_agent_system_prompt(description: &str, subagent_type: &str) -> String {
    let mut parts = vec![
        SYSTEM_PROMPT.trim().to_string(),
        "You are running as a delegated subagent in a fresh, isolated conversation.".into(),
        "Your job is to complete exactly the parent's task efficiently and report the result back. You share the same working directory, configuration, sandbox policy, and approval channel, but you do not inherit the parent's conversation transcript.".into(),
        "Do not address the human directly unless the task explicitly requires it.".into(),
        "Do not repeat the assignment or provide progress chatter. Work autonomously with the tools available to your profile. If blocked, return the precise blocker to the parent.".into(),
        "When you finish, return a concise result with concrete findings or outputs.".into(),
    ];
    if !description.trim().is_empty() {
        parts.push(format!("Task description: {}", description.trim()));
    }
    if !subagent_type.trim().is_empty() {
        parts.push(format!("Requested subagent type: {}", subagent_type.trim()));
    }
    parts.join("\n\n")
}

fn task_is_terminal(paths: &AppPaths, task_id: &str) -> bool {
    tasks::find_task_by_id(paths, task_id)
        .ok()
        .flatten()
        .and_then(|(_, task)| task.get("status").and_then(Value::as_str).map(normalize_ws))
        .is_some_and(|status| is_terminal_status(&status))
}

fn is_terminal_status(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "killed" | "cancelled")
}

fn openai_tool_schemas() -> Vec<Value> {
    vec![
        tool_schema(
            "Agent",
            "Launch an isolated subagent and return its result. Multiple Agent calls in one response run concurrently, like Claude Code.",
            json!({
                "type": "object",
                "properties": {
                    "description": {"type": "string"},
                    "prompt": {"type": "string"},
                    "subagent_type": {
                        "type": "string",
                        "enum": ["general-purpose", "Explore", "Plan"],
                        "default": "general-purpose",
                        "description": "Explore and Plan are read-only; general-purpose may modify the workspace."
                    },
                    "provider_id": {
                        "type": "string",
                        "default": "inherit",
                        "description": "Provider preset for this subagent (for example inherit, openai, anthropic, deepseek, openrouter, custom). The provider's saved API key is reused."
                    },
                    "model": {
                        "type": "string",
                        "default": "inherit",
                        "description": "Use inherit for the parent's model or provide another model id from the active provider."
                    },
                    "run_in_background": {"type": "boolean", "default": false},
                    "isolation": {"type": "string", "enum": ["local", "remote"], "default": "local"}
                },
                "required": ["description", "prompt"],
                "additionalProperties": false
            }),
        ),
        tool_schema(
            "AskUserQuestion",
            "Ask the user one or more focused multiple-choice questions.",
            json!({
                "type": "object",
                "properties": {
                    "questions": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "question": {"type": "string"},
                                "header": {"type": "string"},
                                "multiSelect": {"type": "boolean", "default": false},
                                "options": {
                                    "type": "array",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "label": {"type": "string"},
                                            "description": {"type": "string"},
                                            "preview": {"type": "string"}
                                        },
                                        "required": ["label", "description"],
                                        "additionalProperties": false
                                    }
                                }
                            },
                            "required": ["question", "header", "options"],
                            "additionalProperties": false
                        }
                    },
                    "timeout_seconds": {"type": "integer", "default": 3600}
                },
                "required": ["questions"],
                "additionalProperties": false
            }),
        ),
        tool_schema(
            "ToolSearch",
            "Find available local tools by keyword or by exact name.",
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "max_results": {"type": "integer", "default": 5}
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        ),
        tool_schema(
            "Read",
            "Read a local file from disk.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "max_chars": {"type": "integer", "default": 20000}
                },
                "required": ["path"],
                "additionalProperties": false
            }),
        ),
        tool_schema(
            "Write",
            "Write or overwrite a local file inside the workspace.",
            json!({
                "type": "object",
                "properties": {"path": {"type": "string"}, "content": {"type": "string"}},
                "required": ["path", "content"],
                "additionalProperties": false
            }),
        ),
        tool_schema(
            "Edit",
            "Replace exact text inside a local workspace file.",
            json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_text": {"type": "string"},
                    "new_text": {"type": "string"},
                    "replace_all": {"type": "boolean", "default": false}
                },
                "required": ["path", "old_text", "new_text"],
                "additionalProperties": false
            }),
        ),
        tool_schema(
            "Glob",
            "Find files by glob pattern inside the workspace.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "path": {"type": "string", "default": "."},
                    "limit": {"type": "integer", "default": 200}
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        ),
        tool_schema(
            "Grep",
            "Search file content with ripgrep inside the workspace.",
            json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "path": {"type": "string", "default": "."},
                    "limit": {"type": "integer", "default": 200}
                },
                "required": ["pattern"],
                "additionalProperties": false
            }),
        ),
        tool_schema(
            "Desktop",
            "Navigate the graphical Linux desktop. Prefer semantic AT-SPI: inspect returns controls with opaque targets; activate/set_text/focus acts without coordinates and returns the updated tree. Use observe and pixel actions only when a control is missing. Normal mode asks for approval; full-access runs automatically; read-only blocks it.",
            json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["inspect", "activate", "set_text", "focus", "observe", "click", "double_click", "move", "type", "key", "scroll", "focus_window"]
                    },
                    "query": {"type": "string", "description": "Optional semantic name/role/action filter"},
                    "limit": {"type": "integer", "default": 140, "minimum": 1, "maximum": 400},
                    "target": {"type": "string", "description": "Opaque a11y: target returned by inspect"},
                    "action_name": {"type": "string", "description": "Optional accessible action name"},
                    "x": {"type": "integer"},
                    "y": {"type": "integer"},
                    "button": {"type": "integer", "default": 1},
                    "text": {"type": "string"},
                    "keys": {"type": "string", "description": "xdotool chord, e.g. ctrl+l, Return, alt+F4"},
                    "amount": {"type": "integer", "description": "Positive scrolls down, negative scrolls up"},
                    "window": {"type": "string", "description": "Window-title pattern"},
                    "screenshot_after": {"type": "boolean", "default": true},
                    "inspect_after": {"type": "boolean", "default": true},
                    "after_query": {"type": "string"},
                    "wait_ms": {"type": "integer", "description": "Default 120 ms for semantic actions and 350 ms for visual actions"}
                },
                "required": ["action"],
                "additionalProperties": false
            }),
        ),
        tool_schema(
            "Node",
            "List paired GnomeAI nodes or execute on one weak/remote device. Models and credentials remain on the Hub. Root is controlled by the per-device policy in the main app; this tool never receives a password.",
            json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["list", "exec"]},
                    "node_id": {"type": "string"},
                    "command": {"type": "string"},
                    "cwd": {"type": "string"},
                    "timeout": {"type": "integer", "default": 60, "minimum": 1, "maximum": 3600},
                    "root": {"type": "boolean", "default": false}
                },
                "required": ["action"],
                "additionalProperties": false
            }),
        ),
        tool_schema(
            "Bash",
            "Run a shell command inside the selected workspace. Normal mode asks the user first; sudo is blocked here.",
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "cwd": {"type": "string", "default": "."},
                    "timeout": {"type": "integer", "default": 20},
                    "run_in_background": {"type": "boolean", "default": false}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        ),
        tool_schema(
            "Sudo",
            "Run one explicitly approved command as root. The local interface collects the password; the model never sees it.",
            json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "cwd": {"type": "string", "default": "."},
                    "timeout": {"type": "integer", "default": 60}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        ),
        tool_schema(
            "Config",
            "Get or set supported local runtime settings.",
            json!({
                "type": "object",
                "properties": {
                    "setting": {"type": "string"},
                    "value": {
                        "anyOf": [
                            {"type": "string"},
                            {"type": "number"},
                            {"type": "boolean"}
                        ]
                    }
                },
                "required": ["setting"],
                "additionalProperties": false
            }),
        ),
        tool_schema(
            "Skill",
            "Discover or activate installed SKILL.md instruction packages, or read one \
             referenced text resource. Skills provide instructions but never permissions.",
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "name": {
                        "type": "string",
                        "description": "Exact installed skill name to activate"
                    },
                    "resource": {
                        "type": "string",
                        "description": "Optional path relative to the named skill, such as references/api.md"
                    },
                    "include_content": {"type": "boolean", "default": true},
                    "max_results": {"type": "integer", "default": 5}
                },
                "additionalProperties": false
            }),
        ),
        tool_schema(
            "Learn",
            "Persist a reusable workflow as a managed skill only when the user explicitly asks to learn or remember it. An optional script is POSIX shell. Learning never executes it.",
            json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "description": {"type": "string"},
                    "instructions": {"type": "string"},
                    "script": {"type": "string"},
                    "platforms": {"type": "array", "items": {"type": "string"}},
                    "replace": {"type": "boolean", "default": false}
                },
                "required": ["name", "description", "instructions"],
                "additionalProperties": false
            }),
        ),
        tool_schema(
            "RunSkill",
            "Execute the declared shell entrypoint of an installed skill locally or on a paired node. This is a separate approved operation; remote root follows the per-device policy.",
            json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "target": {"type": "string", "enum": ["local", "node"], "default": "local"},
                    "node_id": {"type": "string"},
                    "cwd": {"type": "string"},
                    "timeout": {"type": "integer", "default": 120, "minimum": 1, "maximum": 120},
                    "root": {"type": "boolean", "default": false}
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        ),
        tool_schema(
            "StructuredOutput",
            "Return the final answer as structured JSON.",
            json!({"type": "object", "additionalProperties": true}),
        ),
        tool_schema(
            "TodoWrite",
            "Replace the current scoped todo list with a new one.",
            json!({
                "type": "object",
                "properties": {"todos": {"type": "array", "items": {"type": "object"}}},
                "required": ["todos"],
                "additionalProperties": false
            }),
        ),
        tool_schema(
            "TaskCreate",
            "Create a workflow task in the current chat scope.",
            json!({
                "type": "object",
                "properties": {
                    "subject": {"type": "string"},
                    "description": {"type": "string"},
                    "activeForm": {"type": "string"},
                    "metadata": {"type": "object", "additionalProperties": true}
                },
                "required": ["subject", "description"],
                "additionalProperties": false
            }),
        ),
        tool_schema(
            "TaskGet",
            "Read a workflow task by ID.",
            json!({
                "type": "object",
                "properties": {"taskId": {"type": "string"}},
                "required": ["taskId"],
                "additionalProperties": false
            }),
        ),
        tool_schema(
            "TaskList",
            "List workflow tasks for the current chat scope.",
            json!({"type": "object", "properties": {}, "additionalProperties": false}),
        ),
        tool_schema(
            "TaskUpdate",
            "Update fields or status of a workflow task.",
            json!({
                "type": "object",
                "properties": {
                    "taskId": {"type": "string"},
                    "subject": {"type": "string"},
                    "description": {"type": "string"},
                    "activeForm": {"type": "string"},
                    "status": {"type": "string", "enum": ["pending", "in_progress", "running", "completed", "failed", "killed", "blocked", "cancelled", "awaiting_input", "deleted"]},
                    "owner": {"type": "string"},
                    "addBlocks": {"type": "array", "items": {"type": "string"}},
                    "addBlockedBy": {"type": "array", "items": {"type": "string"}},
                    "metadata": {"type": "object", "additionalProperties": true}
                },
                "required": ["taskId"],
                "additionalProperties": false
            }),
        ),
        tool_schema(
            "TaskStop",
            "Stop or cancel a workflow task by ID.",
            json!({
                "type": "object",
                "properties": {
                    "task_id": {"type": "string"},
                    "taskId": {"type": "string"},
                    "shell_id": {"type": "string"}
                },
                "additionalProperties": false
            }),
        ),
        tool_schema(
            "TaskOutput",
            "Read the event history or output of a workflow task.",
            json!({
                "type": "object",
                "properties": {
                    "task_id": {"type": "string"},
                    "taskId": {"type": "string"},
                    "block": {"type": "boolean", "default": true},
                    "timeout": {"type": "integer", "default": 30000}
                },
                "additionalProperties": false
            }),
        ),
        tool_schema(
            "WebSearch",
            "Search the web with Firecrawl and return cited results.",
            json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "allowed_domains": {"type": "array", "items": {"type": "string"}},
                    "blocked_domains": {"type": "array", "items": {"type": "string"}}
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        ),
        tool_schema(
            "WebFetch",
            "Fetch and extract a specific URL with Firecrawl.",
            json!({
                "type": "object",
                "properties": {"url": {"type": "string"}, "prompt": {"type": "string"}},
                "required": ["url"],
                "additionalProperties": false
            }),
        ),
    ]
}

fn tool_schema(name: &str, description: &str, parameters: Value) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": parameters,
        }
    })
}

fn tool_schemas_for(agent_profile: AgentProfile, whatsapp_origin: bool) -> Vec<Value> {
    openai_tool_schemas()
        .into_iter()
        .filter(|schema| {
            schema
                .get("function")
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
                .is_some_and(|name| {
                    agent_profile.allows(name)
                        && !(whatsapp_origin && name == "AskUserQuestion")
                })
        })
        .collect()
}

fn tool_metadata() -> Vec<ToolMeta> {
    vec![
        ToolMeta {
            name: "Agent",
            description: "Launch an isolated Claude Code-style subagent.",
            search_hint: "spawn subagent delegate parallel explore plan background worker",
            aliases: &[],
        },
        ToolMeta {
            name: "AskUserQuestion",
            description: "Ask the user one or more focused multiple-choice questions.",
            search_hint: "clarify with user question multiple choice prompt",
            aliases: &[],
        },
        ToolMeta {
            name: "ToolSearch",
            description: "Find available local tools by keyword or by exact name.",
            search_hint: "discover tools search capabilities list tools",
            aliases: &[],
        },
        ToolMeta {
            name: "Read",
            description: "Read a local file from disk.",
            search_hint: "read file inspect source code open local file",
            aliases: &[],
        },
        ToolMeta {
            name: "Write",
            description: "Write or overwrite a local file inside the workspace.",
            search_hint: "create file save content overwrite file",
            aliases: &[],
        },
        ToolMeta {
            name: "Edit",
            description: "Replace exact text inside a local workspace file.",
            search_hint: "edit file patch replace text in file",
            aliases: &[],
        },
        ToolMeta {
            name: "Glob",
            description: "Find files by glob pattern inside the workspace.",
            search_hint: "find files matching pattern wildcard search",
            aliases: &[],
        },
        ToolMeta {
            name: "Grep",
            description: "Search file content with ripgrep inside the workspace.",
            search_hint: "search text in files ripgrep regex",
            aliases: &[],
        },
        ToolMeta {
            name: "Desktop",
            description: "Inspect and control semantic desktop elements, with screenshot and coordinate fallback.",
            search_hint: "desktop gui accessibility at-spi semantic controls screenshot click mouse keyboard type navigate window automation",
            aliases: &["Computer", "ComputerUse"],
        },
        ToolMeta {
            name: "Node",
            description: "List and control paired lightweight GnomeAI execution nodes.",
            search_hint: "remote node device raspberry pi weak pc distributed execute shell root hub",
            aliases: &["Device", "RemoteDevice"],
        },
        ToolMeta {
            name: "Bash",
            description: "Run an approved shell command inside the selected workspace.",
            search_hint: "run shell command inspect environment terminal command",
            aliases: &[],
        },
        ToolMeta {
            name: "Sudo",
            description: "Run one locally approved command as root without exposing the password to the model.",
            search_hint: "sudo root administrator privileged command package install",
            aliases: &[],
        },
        ToolMeta {
            name: "Config",
            description: "Get or set supported local runtime settings.",
            search_hint: "change runtime settings model endpoint config",
            aliases: &[],
        },
        ToolMeta {
            name: "Skill",
            description: "Search and read local SKILL.md instruction files.",
            search_hint: "discover skills prompt libraries instruction packs",
            aliases: &[],
        },
        ToolMeta {
            name: "Learn",
            description: "Store a user-requested reusable workflow as a managed skill.",
            search_hint: "learn remember reusable workflow create skill executable procedure",
            aliases: &["LearnSkill"],
        },
        ToolMeta {
            name: "RunSkill",
            description: "Run an installed skill entrypoint locally or on a paired node.",
            search_hint: "execute run learned skill script local remote node",
            aliases: &["ExecuteSkill"],
        },
        ToolMeta {
            name: "StructuredOutput",
            description: "Return the final answer as structured JSON.",
            search_hint: "json structured output schema final response",
            aliases: &[],
        },
        ToolMeta {
            name: "TodoWrite",
            description: "Replace the current scoped todo list with a new one.",
            search_hint: "manage checklist todo list task checklist",
            aliases: &[],
        },
        ToolMeta {
            name: "TaskCreate",
            description: "Create a workflow task in the current chat scope.",
            search_hint: "create workflow task work item",
            aliases: &[],
        },
        ToolMeta {
            name: "TaskGet",
            description: "Read a workflow task by ID.",
            search_hint: "get task by id inspect task",
            aliases: &[],
        },
        ToolMeta {
            name: "TaskList",
            description: "List workflow tasks for the current chat scope.",
            search_hint: "list tasks task board workflow tasks",
            aliases: &[],
        },
        ToolMeta {
            name: "TaskUpdate",
            description: "Update fields or status of a workflow task.",
            search_hint: "update task status owner metadata dependencies",
            aliases: &[],
        },
        ToolMeta {
            name: "TaskStop",
            description: "Stop or cancel a workflow task by ID.",
            search_hint: "stop cancel task kill task",
            aliases: &["KillShell"],
        },
        ToolMeta {
            name: "TaskOutput",
            description: "Read the event history or output of a workflow task.",
            search_hint: "task logs history output progress",
            aliases: &["AgentOutputTool", "BashOutputTool"],
        },
        ToolMeta {
            name: "WebSearch",
            description: "Search the web with Firecrawl and return cited results.",
            search_hint: "web search internet search current information",
            aliases: &[],
        },
        ToolMeta {
            name: "WebFetch",
            description: "Fetch and extract a specific URL with Firecrawl.",
            search_hint: "fetch webpage scrape url website content",
            aliases: &[],
        },
    ]
}

fn tool_meta_json(meta: &ToolMeta) -> Value {
    json!({
        "name": meta.name,
        "description": meta.description,
        "search_hint": meta.search_hint,
        "aliases": meta.aliases,
    })
}

fn resolve_workspace_path(
    paths: &AppPaths,
    raw_path: &str,
    allow_outside: bool,
) -> anyhow::Result<PathBuf> {
    let raw_path = raw_path.trim();
    if raw_path.is_empty() {
        bail!("Missing path");
    }
    let base = paths
        .workspace_dir
        .canonicalize()
        .unwrap_or_else(|_| paths.workspace_dir.clone());
    let joined = if Path::new(raw_path).is_absolute() {
        PathBuf::from(raw_path)
    } else {
        paths.workspace_dir.join(raw_path)
    };
    let normalized = normalize_path(&joined);

    if allow_outside {
        return Ok(normalized);
    }

    let check_path = if normalized.exists() {
        normalized.canonicalize().unwrap_or(normalized.clone())
    } else {
        let parent = normalized
            .parent()
            .unwrap_or(&paths.workspace_dir)
            .canonicalize()
            .unwrap_or_else(|_| {
                normalize_path(normalized.parent().unwrap_or(&paths.workspace_dir))
            });
        parent.join(normalized.file_name().unwrap_or_default())
    };
    if !check_path.starts_with(&base) {
        bail!("Path is outside workspace: {}", check_path.display());
    }
    Ok(check_path)
}

fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn collect_glob_matches(
    root: &Path,
    current: &Path,
    matcher: &Regex,
    pattern: &str,
    max: usize,
    matches: &mut Vec<String>,
) -> anyhow::Result<()> {
    if matches.len() >= max || !current.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(current)? {
        let path = entry?.path();
        let rel = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if matcher.is_match(&rel) || (!pattern.contains('/') && matcher.is_match(name)) {
            matches.push(path.to_string_lossy().to_string());
            if matches.len() >= max {
                return Ok(());
            }
        }
        if path.is_dir() {
            collect_glob_matches(root, &path, matcher, pattern, max, matches)?;
        }
    }
    Ok(())
}

fn wildcard_regex(pattern: &str) -> anyhow::Result<Regex> {
    let mut out = String::from("^");
    for ch in pattern.chars() {
        match ch {
            '*' => out.push_str(".*"),
            '?' => out.push('.'),
            '.' | '+' | '(' | ')' | '|' | '^' | '$' | '{' | '}' | '[' | ']' | '\\' => {
                out.push('\\');
                out.push(ch);
            }
            other => out.push(other),
        }
    }
    out.push('$');
    Ok(Regex::new(&out)?)
}

fn domain_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(|item| {
                    item.trim()
                        .trim_start_matches("https://")
                        .trim_start_matches("http://")
                        .split('/')
                        .next()
                        .unwrap_or("")
                        .to_string()
                })
                .filter(|item| !item.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn config_tool_settings() -> Vec<(&'static str, &'static str)> {
    vec![
        ("provider_id", "Selected shared provider preset."),
        ("default_model", "Default model used for chat requests."),
        ("llama_base_url", "Base URL for llama-server."),
        (
            "llama_api_mode",
            "Request style for llama-server (chat or completion).",
        ),
        (
            "llama_timeout",
            "Timeout for llama-server requests in seconds.",
        ),
        (
            "llama_max_tokens",
            "Maximum output tokens requested from llama-server.",
        ),
        ("firecrawl_api_url", "Base URL for Firecrawl."),
        (
            "web_search_enabled",
            "Enable or disable WebSearch/WebFetch and lazy local Firecrawl startup.",
        ),
        (
            "firecrawl_count",
            "Number of search results to request from Firecrawl.",
        ),
        (
            "firecrawl_extract_count",
            "How many pages to scrape after search.",
        ),
        (
            "firecrawl_timeout_ms",
            "Timeout for Firecrawl requests in milliseconds.",
        ),
        (
            "firecrawl_excerpt_chars",
            "Maximum excerpt size kept from scraped pages.",
        ),
        (
            "tool_loop_max_steps",
            "Maximum number of tool-calling rounds.",
        ),
        ("agent_max_depth", "Maximum nested Agent tool depth."),
        (
            "agent_max_concurrent",
            "Maximum number of subagents that may run concurrently.",
        ),
        (
            "remote_agent_api_url",
            "Optional launcher URL for remote agents.",
        ),
        (
            "history_window",
            "How many recent messages are included in prompt context.",
        ),
        (
            "memory_enabled",
            "Enable or disable cross-conversation memory extraction and prompt injection.",
        ),
        (
            "memory_max_age_days",
            "Maximum memory age in days; zero disables the age limit.",
        ),
        (
            "memory_max_facts_in_prompt",
            "Maximum remembered facts injected into each prompt.",
        ),
        (
            "memory_max_recent_summaries_in_prompt",
            "Maximum related conversation summaries injected into each prompt.",
        ),
        (
            "memory_extract_message_window",
            "How many recent messages are scanned when extracting new memories.",
        ),
        (
            "memory_max_existing_facts_for_extraction",
            "How many existing facts are shown to the memory extractor model.",
        ),
        (
            "memory_max_recent_summaries_stored",
            "Maximum recent conversation summaries kept in local memory storage.",
        ),
        (
            "memory_max_facts_stored",
            "Maximum persistent facts kept in local memory storage.",
        ),
    ]
}

fn coerce_config_value(setting: &str, raw: &Value, previous: &Value) -> anyhow::Result<Value> {
    match previous {
        Value::Bool(_) => value_bool_from_value(raw)
            .map(Value::Bool)
            .ok_or_else(|| anyhow!("{setting} expects a boolean value")),
        Value::Number(_) => value_u64_from_value(raw)
            .map(|value| json!(value))
            .ok_or_else(|| anyhow!("{setting} expects an integer value")),
        _ => Ok(json!(match raw {
            Value::String(text) => text.trim().to_string(),
            Value::Null => String::new(),
            other => other.to_string(),
        })),
    }
}

fn required_string(args: &Map<String, Value>, key: &str) -> anyhow::Result<String> {
    value_string(args, key)
        .filter(|item| !item.trim().is_empty())
        .ok_or_else(|| anyhow!("Missing {key}"))
}

fn value_string(args: &Map<String, Value>, key: &str) -> Option<String> {
    args.get(key).and_then(|value| match value {
        Value::String(text) => Some(text.trim().to_string()),
        Value::Null => None,
        other => Some(other.to_string()),
    })
}

fn value_bool(args: &Map<String, Value>, key: &str) -> Option<bool> {
    args.get(key).and_then(value_bool_from_value)
}

fn value_bool_from_value(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(item) => Some(*item),
        Value::String(text) => match text.trim().to_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Some(true),
            "0" | "false" | "no" | "off" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn value_u64(args: &Map<String, Value>, key: &str) -> Option<u64> {
    args.get(key).and_then(value_u64_from_value)
}

fn value_u64_from_value(value: &Value) -> Option<u64> {
    match value {
        Value::Number(item) => item.as_u64(),
        Value::String(text) => text.trim().parse::<u64>().ok(),
        _ => None,
    }
}

fn validate_bash_command(command: &str) -> anyhow::Result<()> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        bail!("Missing command");
    }
    if trimmed.len() > 8_000 {
        bail!("Bash command is too long");
    }
    if trimmed.contains('\0') {
        bail!("Bash command contains a NUL byte");
    }
    if bash_background_operator().is_match(trimmed) {
        bail!("Shell background operators are blocked; use run_in_background instead");
    }
    Ok(())
}

fn bash_background_operator() -> &'static Regex {
    static BACKGROUND: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"(^|[^&])&($|[^&])").unwrap());
    &BACKGROUND
}

fn normalize_ws(text: &str) -> String {
    Regex::new(r"\s+")
        .unwrap()
        .replace_all(text, " ")
        .trim()
        .to_string()
}

fn is_whatsapp_scope(scope_key: &str) -> bool {
    scope_key.starts_with("wa_") || scope_key.starts_with("whatsapp_")
}

fn source_channel_for_scope(scope_key: &str) -> &'static str {
    if is_whatsapp_scope(scope_key) {
        "whatsapp"
    } else {
        "webtool"
    }
}

fn runtime_timestamp() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn short_command(command: &str, max_chars: usize) -> String {
    let normalized = normalize_ws(command);
    let chars = normalized.chars().collect::<Vec<_>>();
    if chars.len() <= max_chars {
        normalized
    } else {
        format!(
            "{}...",
            chars[..max_chars.saturating_sub(3)]
                .iter()
                .collect::<String>()
        )
    }
}

fn tail(text: &str, max_chars: usize) -> String {
    let chars = text.chars().collect::<Vec<_>>();
    let start = chars.len().saturating_sub(max_chars);
    chars[start..].iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Json, Router,
        routing::{get, post},
    };

    #[test]
    fn whatsapp_never_advertises_the_blocking_question_widget() {
        let whatsapp = tool_schemas_for(AgentProfile::Root, true);
        let desktop = tool_schemas_for(AgentProfile::Root, false);
        let has_question_widget = |schemas: &[Value]| {
            schemas.iter().any(|schema| {
                schema.pointer("/function/name").and_then(Value::as_str)
                    == Some("AskUserQuestion")
            })
        };

        assert!(!has_question_widget(&whatsapp));
        assert!(has_question_widget(&desktop));
    }

    #[test]
    fn parses_markdown_web_search_as_a_real_tool_call() {
        let calls = parse_textual_tool_calls(
            r#"**WebSearch** (query: "IBM Series/1 programming interface", language: Romanian)"#,
        );
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].pointer("/function/name"),
            Some(&json!("WebSearch"))
        );
        let args: Value = serde_json::from_str(
            calls[0]
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(args["query"], "IBM Series/1 programming interface");
        assert_eq!(args["language"], "Romanian");
    }

    #[test]
    fn parses_markdown_task_output_and_aliases() {
        let calls = parse_textual_tool_calls(
            "**AgentOutputTool** (taskId : websearch-ibm-series-1, block: true)",
        );
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].pointer("/function/name"),
            Some(&json!("TaskOutput"))
        );
        let args: Value = serde_json::from_str(
            calls[0]
                .pointer("/function/arguments")
                .and_then(Value::as_str)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(args["taskId"], "websearch-ibm-series-1");
        assert_eq!(args["block"], true);
    }

    #[test]
    fn does_not_execute_unknown_markdown_labels() {
        assert!(parse_textual_tool_calls("**Assistant** (reply: hello)").is_empty());
        assert!(parse_textual_tool_calls("**Important** (note: hello)").is_empty());
    }

    #[test]
    fn unwraps_markdown_assistant_reply() {
        assert_eq!(
            unwrap_textual_assistant_reply(
                r#"**Assistant** (reply: "Salut! Sunt aici și gata să te ajut.")"#,
            ),
            "Salut! Sunt aici și gata să te ajut."
        );
    }

    #[test]
    fn web_execution_policy_uses_the_selected_mode_and_workspace() {
        let mut cfg = AppConfig::default();
        cfg.web_sandbox_mode = "full-access".into();
        let workspace = PathBuf::from("/tmp/example-workspace");
        let policy = web_sandbox_policy(&cfg, workspace.clone(), 42);
        assert_eq!(policy.mode, SandboxMode::FullAccess);
        assert_eq!(policy.cwd, workspace);
        assert_eq!(policy.timeout_ms, 42_000);
    }

    #[test]
    fn subagent_profiles_enforce_read_only_roles() {
        assert!(AgentProfile::GeneralPurpose.allows("Edit"));
        assert!(AgentProfile::GeneralPurpose.allows("Bash"));
        assert!(!AgentProfile::GeneralPurpose.allows("Config"));
        assert!(AgentProfile::Explore.allows("Read"));
        assert!(AgentProfile::Explore.allows("WebSearch"));
        assert!(!AgentProfile::Explore.allows("Write"));
        assert!(!AgentProfile::Explore.allows("Bash"));
        assert!(AgentProfile::Plan.allows("Agent"));
        assert!(!AgentProfile::Plan.allows("Edit"));
    }

    #[test]
    fn every_subagent_can_select_its_own_saved_provider_and_model() {
        let mut cfg = AppConfig::default();
        cfg.provider_id = "custom".into();
        cfg.default_model = "local-parent".into();
        cfg.remember_provider_api_key("openrouter", Some("sk-openrouter-saved"));
        cfg.normalize();

        let (agent_cfg, provider, model) = resolve_agent_provider(
            &cfg,
            "local-parent",
            "openrouter",
            Some("deepseek/deepseek-v4"),
        )
        .unwrap();
        assert_eq!(provider, "openrouter");
        assert_eq!(model, "deepseek/deepseek-v4");
        assert_eq!(agent_cfg.provider_id, "openrouter");
        assert_eq!(agent_cfg.llama_api_key, "sk-openrouter-saved");
        assert_eq!(agent_cfg.llama_base_url, "https://openrouter.ai/api/v1");
        assert_eq!(cfg.provider_id, "custom");
    }

    #[test]
    fn subagent_provider_without_saved_key_is_rejected() {
        let cfg = AppConfig::default();
        let error = resolve_agent_provider(&cfg, &cfg.default_model, "anthropic", None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("no saved API key"), "{error}");
    }

    #[test]
    fn file_tools_are_rooted_in_the_selected_workspace() {
        let root =
            std::env::temp_dir().join(format!("gnomef-rs-path-test-{}", Uuid::new_v4().simple()));
        let workspace = root.join("project");
        let app_home = root.join("state");
        std::fs::create_dir_all(&workspace).unwrap();
        let mut paths = AppPaths::new(app_home).unwrap();
        paths.workspace_dir = workspace.canonicalize().unwrap();

        assert_eq!(
            resolve_workspace_path(&paths, ".", false).unwrap(),
            paths.workspace_dir
        );
        assert!(resolve_workspace_path(&paths, "../outside", false).is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn config_tool_updates_shared_state() {
        let root =
            std::env::temp_dir().join(format!("gnomef-rs-config-test-{}", Uuid::new_v4().simple()));
        let paths = AppPaths::new(root.clone()).unwrap();
        let cfg = AppConfig::default();
        let shared = std::sync::Arc::new(RwLock::new(cfg.clone()));
        let mut args = Map::new();
        args.insert("setting".into(), json!("history_window"));
        args.insert("value".into(), json!(11));

        let response = tool_config(&cfg, &paths, &args, Some(shared.clone()))
            .await
            .unwrap();

        assert_eq!(
            response.get("operation").and_then(Value::as_str),
            Some("set")
        );
        assert_eq!(shared.read().await.history_window, 11);
        let saved = AppConfig::load(&paths.config_file).unwrap();
        assert_eq!(saved.history_window, 11);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_tool_loop_is_not_bounded_by_a_round_count_out_of_the_box() {
        let mut cfg = AppConfig::default();
        assert_eq!(
            cfg.tool_loop_max_steps, 0,
            "a fixed round cap ends useful work mid-task; the loop is bounded \
             by context and by the user's interrupt instead"
        );

        // `normalize` runs on every load and on every Config tool write. A
        // clamp of `1..=64` here would quietly turn "unlimited" into a
        // one-round limit, which is worse than the cap it replaced.
        cfg.normalize();
        assert_eq!(
            cfg.tool_loop_max_steps, 0,
            "normalize must not clamp the unlimited sentinel up to 1"
        );
    }

    #[test]
    fn an_explicit_round_cap_survives_normalization_and_stays_bounded() {
        let mut cfg = AppConfig::default();
        cfg.tool_loop_max_steps = 12;
        cfg.normalize();
        assert_eq!(cfg.tool_loop_max_steps, 12);

        cfg.tool_loop_max_steps = 9_000;
        cfg.normalize();
        assert_eq!(cfg.tool_loop_max_steps, 64, "the upper bound still applies");
    }

    #[test]
    fn context_budget_measures_the_whole_conversation() {
        let messages = vec![
            json!({"role": "system", "content": "x".repeat(1_000)}),
            json!({"role": "user", "content": "y".repeat(1_000)}),
        ];
        let measured = context_bytes(&messages);
        assert!(measured > 2_000, "both messages must count: {measured}");
        assert!(
            measured < MAX_LOOP_CONTEXT_BYTES,
            "an ordinary exchange must not trip the budget"
        );
    }

    /// A conversation shaped like the loop actually builds one: seed, then
    /// rounds of `assistant(tool_calls)` followed by their tool results.
    fn conversation(rounds: usize, filler: usize) -> Vec<Value> {
        let mut messages = vec![
            json!({"role": "system", "content": "system"}),
            json!({"role": "user", "content": "the original request"}),
        ];
        for index in 0..rounds {
            messages.push(json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": format!("call_{index}"),
                    "type": "function",
                    "function": {"name": "Read", "arguments": "{}"},
                }],
            }));
            messages.push(json!({
                "role": "tool",
                "tool_call_id": format!("call_{index}"),
                "content": "x".repeat(filler),
            }));
        }
        messages
    }

    #[test]
    fn compaction_never_orphans_a_tool_result() {
        // keep_recent lands mid-round on purpose: the cut must walk back off
        // the tool result, or the next request carries a dangling tool call.
        for keep_recent in 1..12 {
            let messages = conversation(10, 4_000);
            let Some(plan) = plan_message_compaction(&messages, 1_000, keep_recent) else {
                continue;
            };
            assert_ne!(
                message_role(&messages[plan.range.end]),
                "tool",
                "keep_recent={keep_recent} left a tool result as the first surviving message"
            );
        }
    }

    #[test]
    fn compaction_keeps_the_system_prompt_and_the_original_request() {
        let messages = conversation(10, 4_000);
        let plan = plan_message_compaction(&messages, 1_000, COMPACTION_KEEP_RECENT).unwrap();
        assert_eq!(
            plan.range.start, 2,
            "the seed is pinned: summarise the goal away and the model solves a different problem"
        );
        assert!(plan.freed_bytes > 0);
    }

    #[test]
    fn compaction_does_not_run_while_the_conversation_fits() {
        let messages = conversation(2, 10);
        assert_eq!(
            plan_message_compaction(&messages, MAX_LOOP_CONTEXT_BYTES, COMPACTION_KEEP_RECENT),
            None
        );
    }

    #[test]
    fn compaction_declines_when_only_recent_rounds_remain() {
        // Everything is inside the keep-recent window, so there is nothing safe
        // to fold; the caller must stop rather than truncate blindly.
        let messages = conversation(2, 200_000);
        assert_eq!(plan_message_compaction(&messages, 1_000, 6), None);
    }

    #[test]
    fn compaction_always_makes_progress() {
        // Folding one message into one summary would let the loop spin forever.
        for rounds in 1..8 {
            let messages = conversation(rounds, 8_000);
            if let Some(plan) = plan_message_compaction(&messages, 1_000, COMPACTION_KEEP_RECENT) {
                assert!(
                    plan.range.len() >= 2,
                    "rounds={rounds} folded {} message(s), which frees nothing",
                    plan.range.len()
                );
            }
        }
    }

    #[test]
    fn context_budget_stops_a_loop_that_keeps_growing() {
        let huge = vec![json!({"role": "tool", "content": "z".repeat(MAX_LOOP_CONTEXT_BYTES)})];
        assert!(context_bytes(&huge) > MAX_LOOP_CONTEXT_BYTES);
    }

    #[test]
    fn tool_summaries_are_readable_without_parsing_json() {
        let args = Map::from_iter([("command".into(), json!("cargo test --lib"))]);
        assert_eq!(summarize_call("Bash", &args), "cargo test --lib");

        let args = Map::from_iter([("path".into(), json!("src/main.rs"))]);
        assert_eq!(summarize_call("Edit", &args), "edit src/main.rs");

        // An unmapped tool still names itself rather than rendering "?".
        assert_eq!(summarize_call("TaskList", &Map::new()), "TaskList");
    }

    /// Waits for `expected` approval requests to register, so the assertions
    /// below never depend on task scheduling order.
    async fn wait_for_pending(pending: &PendingApprovals, expected: usize) -> Value {
        for _ in 0..200 {
            let view = pending.public_view().await;
            let count = view["requests"].as_array().map_or(0, |items| items.len());
            if count >= expected {
                return view;
            }
            sleep(Duration::from_millis(5)).await;
        }
        panic!("only saw fewer than {expected} pending approval(s)");
    }

    #[test]
    fn subagents_get_their_own_approval_scope_but_share_the_conversation() {
        assert_eq!(approval_scope_for("chat_7", None), "chat_7");
        assert_eq!(approval_scope_for("chat_7", Some("a1")), "chat_7#a1");
        assert_ne!(
            approval_scope_for("chat_7", Some("a1")),
            approval_scope_for("chat_7", Some("a2"))
        );
        // The WhatsApp prefix drives implicit authorization and task source
        // attribution, so it has to survive the subagent suffix.
        assert_eq!(
            source_channel_for_scope(&approval_scope_for("wa_123", Some("a1"))),
            "whatsapp"
        );
    }

    #[tokio::test]
    async fn whatsapp_standard_tools_never_register_a_desktop_approval() {
        let test_root = std::env::temp_dir().join(format!(
            "gnomeai-whatsapp-approval-test-{}",
            Uuid::new_v4().simple()
        ));
        let mut paths = AppPaths::new(test_root.clone()).unwrap();
        paths.workspace_dir = test_root.clone();
        let pending = PendingApprovals::default();
        let cfg = AppConfig::default();

        authorize_standard(
            &cfg,
            &pending,
            "wa_40700000000_s_whatsapp_net#a1",
            "Bash",
            "find /home/user -name '*.jpg'",
            "may inspect user files",
            &paths,
        )
        .await
        .unwrap();

        let view = pending.public_view().await;
        assert_eq!(view["pending"], false);
        assert!(view["requests"].as_array().unwrap().is_empty());
        std::fs::remove_dir_all(test_root).unwrap();
    }

    #[tokio::test]
    async fn read_only_still_blocks_whatsapp_mutations_without_prompting() {
        let test_root = std::env::temp_dir().join(format!(
            "gnomeai-whatsapp-read-only-test-{}",
            Uuid::new_v4().simple()
        ));
        let mut paths = AppPaths::new(test_root.clone()).unwrap();
        paths.workspace_dir = test_root.clone();
        let pending = PendingApprovals::default();
        let mut cfg = AppConfig::default();
        cfg.web_sandbox_mode = "read-only".into();

        let result = authorize_standard(
            &cfg,
            &pending,
            "wa_40700000000_s_whatsapp_net",
            "Write",
            "result.txt",
            "writes a file",
            &paths,
        )
        .await;

        assert!(result.unwrap_err().to_string().contains("read-only"));
        assert_eq!(pending.public_view().await["pending"], false);
        std::fs::remove_dir_all(test_root).unwrap();
    }

    #[tokio::test]
    async fn concurrent_subagents_do_not_reject_each_others_approvals() {
        let pending = PendingApprovals::default();
        let handles: Vec<_> = ["a1", "a2"]
            .into_iter()
            .map(|agent| {
                let pending = pending.clone();
                let scope = approval_scope_for("chat_7", Some(agent));
                tokio::spawn(async move {
                    pending
                        .request_standard(&scope, "Write", "src/lib.rs", "writes a file", "/ws")
                        .await
                })
            })
            .collect();

        let view = wait_for_pending(&pending, 2).await;
        for request in view["requests"].as_array().unwrap() {
            pending
                .answer(
                    request["id"].as_str().unwrap(),
                    crate::web_approvals::ApprovalAnswerPayload {
                        decision: "allow".into(),
                        credential: None,
                        remember: false,
                    },
                )
                .await
                .unwrap();
        }

        for handle in handles {
            assert!(
                handle.await.unwrap().unwrap(),
                "a fan-out of subagents must not have one prompt reject the other"
            );
        }
    }

    #[tokio::test]
    async fn one_agent_still_gets_one_prompt_at_a_time() {
        let pending = PendingApprovals::default();
        let scope = approval_scope_for("chat_7", None);
        let worker = {
            let pending = pending.clone();
            let scope = scope.clone();
            tokio::spawn(async move {
                pending
                    .request_standard(&scope, "Bash", "cargo test", "runs tests", "/ws")
                    .await
            })
        };
        wait_for_pending(&pending, 1).await;

        // Same scope, still serialized: one agent must not stack dialogs.
        let second = pending
            .request_standard(&scope, "Bash", "rm -rf build", "deletes files", "/ws")
            .await;
        assert!(matches!(
            second,
            Err(crate::web_approvals::PendingApprovalError::AlreadyPending)
        ));

        let view = pending.public_view().await;
        pending
            .answer(
                view["request"]["id"].as_str().unwrap(),
                crate::web_approvals::ApprovalAnswerPayload {
                    decision: "deny".into(),
                    credential: None,
                    remember: false,
                },
            )
            .await
            .unwrap();
        assert!(!worker.await.unwrap().unwrap());
    }

    #[test]
    fn bash_validation_keeps_background_processes_managed() {
        assert!(validate_bash_command("printf ok").is_ok());
        assert!(validate_bash_command("rm -rf ./build").is_ok());
        assert!(validate_bash_command("printf ok > result.txt").is_ok());
        assert!(validate_bash_command("sleep 1 &").is_err());
    }

    #[test]
    fn empty_response_fallback_uses_successful_tool_observations() {
        let root = std::env::temp_dir().join(format!(
            "gnomef-rs-empty-fallback-test-{}",
            Uuid::new_v4().simple()
        ));
        let paths = AppPaths::new(root.clone()).unwrap();
        let profile = RuntimeProfile::detect(&paths);
        let args = Map::from_iter([("command".into(), json!("nvidia-smi"))]);
        let result = json!({
            "command": "nvidia-smi",
            "cwd": ".",
            "exit_code": 0,
            "stdout": "RTX 4070",
            "stderr": "",
        });
        let observation = ToolObservation::from_success(&profile, "Bash", &args, &result);
        let fallback = empty_response_fallback(&[observation]).unwrap();
        assert!(fallback.contains("The model did not return a final answer"));
        assert!(fallback.contains("nvidia-smi"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn remote_agent_launches_and_polls_to_completion() {
        let root = std::env::temp_dir().join(format!(
            "gnomef-rs-remote-agent-test-{}",
            Uuid::new_v4().simple()
        ));
        let paths = AppPaths::new(root.clone()).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let poll_url = format!("http://{addr}/poll");
        let app = Router::new()
            .route(
                "/launch",
                post({
                    let poll_url = poll_url.clone();
                    move || {
                        let poll_url = poll_url.clone();
                        async move {
                            Json(json!({
                                "session_url": "http://session.test/agent",
                                "poll_url": poll_url,
                            }))
                        }
                    }
                }),
            )
            .route(
                "/poll",
                get(|| async {
                    Json(json!({
                        "status": "completed",
                        "append_output": "remote-ok\n",
                        "result": "remote done",
                    }))
                }),
            );
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await;
        });

        let mut cfg = AppConfig::default();
        cfg.remote_agent_api_url = format!("http://{addr}");
        let runtime = RuntimeHandles::default();
        let launched = launch_remote_agent(
            &cfg,
            &paths,
            &runtime,
            "test",
            "check remote",
            "Remote check",
            "general-purpose",
            &cfg.default_model,
            None,
            1,
            &cfg.provider_id,
        )
        .await
        .unwrap();
        let task_id = launched
            .get("taskId")
            .and_then(Value::as_str)
            .unwrap()
            .to_string();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let output = loop {
            let output = tasks::task_output(&paths, "test", &task_id, false).unwrap();
            let status = output
                .get("task")
                .and_then(|task| task.get("status"))
                .and_then(Value::as_str)
                .unwrap_or("");
            if status == "completed" {
                break output;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "remote agent did not complete: {output}"
            );
            sleep(Duration::from_millis(50)).await;
        };

        let log = output
            .get("task")
            .and_then(|task| task.get("output"))
            .and_then(Value::as_str)
            .unwrap_or("");
        assert!(log.contains("remote-ok"), "{log}");
        let result = output
            .get("task")
            .and_then(|task| task.get("result"))
            .and_then(Value::as_str)
            .unwrap_or("");
        assert_eq!(result, "remote done");
        let _ = shutdown_tx.send(());
        let _ = server.await;
        let _ = std::fs::remove_dir_all(root);
    }
}
