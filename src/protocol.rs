//! The wire between the coding-agent core and every interface.
//!
//! One queue pair: interfaces send `Op`, the core emits `Event`. Nothing else
//! crosses the boundary — no shared state, no direct calls into the core, no
//! agent logic living inside an HTTP handler.
//!
//! This is the piece to build while the codebase is still small. Once the
//! agent loop leaks into interface handlers, replacing one frontend requires
//! rewriting the core as well.
//!
//! The native GUI uses Tokio channels in-process. The protocol remains
//! serializable so headless or remote clients can be added without coupling
//! them to the agent implementation.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;
use zeroize::Zeroize;

use crate::config::McpServerConfig;

/// A serialisable secret that is always redacted from diagnostics.
#[derive(Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretString(String);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("[REDACTED]")
    }
}

impl Drop for SecretString {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

// ---------------------------------------------------------------------------
// Interface -> core
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    /// Local authenticated device adapter. Remote peers can only request the
    /// limited session actions enforced by the runtime, not arbitrary UI ops.
    DeviceRequest {
        request_id: String,
        peer_id: String,
        action: String,
        payload: serde_json::Value,
    },
    SubmitAttachmentTo {
        session_id: String,
        path: PathBuf,
        text: String,
    },
    /// Submit several local files to a specific conversation in one message.
    /// The multi-file form of `SubmitAttachmentTo`, used when a device runs a
    /// session that another interface is driving.
    SubmitAttachmentsTo {
        session_id: String,
        paths: Vec<PathBuf>,
        text: String,
    },
    SetBrave {
        api_key: Option<SecretString>,
        mode: String,
    },
    /// Start a turn with this user message.
    Submit {
        text: String,
    },

    /// Submit text together with one local attachment. The core performs all
    /// file validation and extraction so every desktop frontend gets the same
    /// PDF, Office, source-code and image behavior.
    SubmitAttachment {
        text: String,
        path: PathBuf,
    },

    /// Submit text together with any number of local attachments.
    ///
    /// One message, many files: the core reads each one in order and passes a
    /// single combined prompt to the model, so the turn stays one turn and the
    /// transcript one user bubble. `SubmitAttachment` remains for a single file.
    SubmitAttachments {
        text: String,
        paths: Vec<PathBuf>,
    },

    /// Start a fresh session in the current workspace.
    NewSession,

    /// Move the agent to another workspace. The core validates the directory,
    /// rebuilds every path-bound tool/provider, and starts a fresh session
    /// there so project context cannot leak across repositories.
    SetWorkspace {
        path: PathBuf,
    },

    /// Stop the current turn. Must be handled even mid-tool-call: the point of
    /// an interrupt is that it works when the agent is busy, which is the only
    /// time anyone presses it.
    Interrupt,

    /// Answer a pending `ApprovalRequest`.
    Approve {
        call_id: String,
        decision: Decision,
    },

    /// Answer a local sudo credential prompt. The credential is transported
    /// only on the in-process channel and its memory is zeroed on drop.
    ProvidePrivilegeCredential {
        request_id: String,
        credential: Option<SecretString>,
        remember: bool,
    },

    /// Force context compaction now instead of waiting for the budget.
    Compact,

    /// Undo every patch in this session.
    Rollback,

    /// Show the accumulated, non-reverted patch log.
    ShowDiff,

    /// Swap model mid-session.
    SetModel {
        model: String,
    },

    /// Persist a provider/model reasoning-effort preference. `default` lets
    /// the selected provider keep its own default.
    SetReasoningEffort {
        effort: String,
    },

    /// Select a hosted provider or one of the official account-backed CLIs.
    /// The core persists API credentials in its owner-only settings file;
    /// they are never echoed back in an event.
    SetProvider {
        provider_id: String,
        api_key: Option<SecretString>,
        base_url: Option<String>,
    },

    /// Persist the default provider/model used when an Agent delegation does
    /// not explicitly override either field.
    SetSubagentDefaults {
        enabled: bool,
        provider_id: String,
        model: String,
        #[serde(default = "default_reasoning_effort")]
        reasoning_effort: String,
    },

    /// Start the official account authentication flow for a provider. API-key
    /// providers continue to use `SetProvider` directly.
    LoginProvider {
        provider_id: String,
    },

    /// Enable or disable all web-search and web-fetch tools. The setting is
    /// persisted outside the conversation transcript.
    SetWebSearch {
        enabled: bool,
    },

    /// Configure the local Firecrawl deployment on Android (Termux side).
    /// `launcher_path` selects the `gnomeai-firecrawl` script; `None` keeps
    /// the saved value.
    SetFirecrawl {
        launcher_path: Option<String>,
        api_url: Option<String>,
    },

    /// Replace the generic MCP server list and rebuild the shared tool
    /// registry. Each enabled server is connected independently.
    SetMcpServers {
        servers: Vec<McpServerConfig>,
    },

    /// Change the sandbox policy mid-session. Narrowing takes effect on the
    /// next command; widening should require the user to confirm.
    SetSandbox {
        mode: String,
    },

    /// Persist the WhatsApp bridge settings used by the native background
    /// service. The GUI asks that service to reload only after this succeeds.
    SetWhatsApp {
        enabled: bool,
        assistant_name: String,
        has_own_number: bool,
        allowed_jids: Vec<String>,
    },

    /// Configure the listener used by init-agnostic lightweight nodes. A
    /// listener change takes effect after restarting the native application.
    SetNodeHub {
        enabled: bool,
        bind: String,
        port: u16,
    },

    /// Ask the core for the recent session list; answered with
    /// `Event::SessionList`.
    ListSessions,

    /// Switch the live agent to an existing session, restoring its workspace
    /// and replaying its visible history.
    ResumeSession {
        id: String,
    },

    /// Branch the current session at its latest turn and switch to the copy.
    ForkSession,

    RenameSession {
        id: String,
        title: String,
    },

    DeleteSession {
        id: String,
    },

    /// Show the shared cross-conversation memory.
    MemoryShow,

    /// Show memory-engine health: counts, embeddings, last dream cycle.
    MemoryStatus,

    /// Wipe the shared cross-conversation memory store.
    MemoryClear,

    /// Enable or disable the shared cross-conversation memory.
    MemorySet {
        enabled: bool,
    },

    /// Run a consolidation ("dreaming") cycle now. `dry_run` reports the
    /// planned operations without writing anything.
    MemoryDream {
        dry_run: bool,
    },

    /// Re-embed every stored fact with the current embedding model.
    MemoryReindex,

    /// Soft-forget one fact by id (kept on disk as status `forgotten`).
    MemoryForget {
        id: String,
    },

    /// List, inspect and manage Agent Skills compatible with SKILL.md.
    SkillsList,
    SkillInspect {
        name: String,
    },
    SkillActivate {
        name: String,
    },
    SkillInstall {
        source: String,
    },
    SkillUpdate {
        name: String,
    },
    SkillVerify {
        name: String,
    },
    SkillRemove {
        name: String,
    },

    /// Recursive harness adaptation (the harness2 method). `task` is the work
    /// to adapt against, as the user states it; the run writes its evidence,
    /// rollouts, selection and summary under the harness2 runs root.
    Harness2 {
        /// A plain-language statement of the task to adapt for.
        task: String,
        /// Candidate harnesses drawn per (mode, domain) -- the paper's k.
        #[serde(default = "default_harness2_k")]
        k: u32,
        /// `parallel` (agg), `sequential` (seq) or `both`.
        #[serde(default = "default_harness2_mode")]
        mode: String,
        /// Emit the plan and the evidence layout without calling a model.
        #[serde(default)]
        dry_run: bool,
    },

    /// The desktop's dry-run request: report the plan, the `4k`/`3k` budget,
    /// the editable surface and the runs root without calling a model.
    ///
    /// The interface already emits the tag `harness2_plan`, so it gets its own
    /// variant rather than a flag on [`Op::Harness2`]: which tag arrived is
    /// what decides whether a sweep may start, and a client must not be able
    /// to ask for the plan and get sixteen autonomous sessions instead.
    Harness2Plan {
        /// A plain-language statement of the task to adapt for.
        task: String,
        /// Candidate harnesses drawn per (mode, domain) -- the paper's k.
        #[serde(default = "default_harness2_k")]
        k: u32,
        /// `parallel` (agg), `sequential` (seq) or `both`.
        #[serde(default = "default_harness2_mode")]
        mode: String,
    },

    /// The desktop's real-sweep request: propose, roll out, compose and adopt
    /// for one task. Every rollout is a genuinely autonomous session, so the
    /// core reports the plan up front and streams progress as notices.
    Harness2Start {
        /// A plain-language statement of the task to adapt for.
        task: String,
        /// Candidate harnesses drawn per (mode, domain) -- the paper's k.
        #[serde(default = "default_harness2_k")]
        k: u32,
        /// `parallel` (agg), `sequential` (seq) or `both`.
        #[serde(default = "default_harness2_mode")]
        mode: String,
    },

    /// Run environment diagnostics and report the results as a notice.
    Doctor,

    // -----------------------------------------------------------------------
    // Messenger (P2P, Tor-only, no server)
    //
    // These are deliberately separate from the device-mesh ops: a messenger
    // account is a distinct identity with its own onion service, and nothing
    // here reads or derives a device key.
    // -----------------------------------------------------------------------
    /// Create another local messenger identity. `pro` only raises this
    /// installation's local limits (accounts and per-file ceiling).
    MessengerCreateAccount {
        display_name: String,
        #[serde(default)]
        pro: bool,
    },
    MessengerListAccounts,
    /// Instant switch of the account the UI is showing. No logout, no restart.
    MessengerSwitchAccount {
        account_id: String,
    },
    MessengerDeleteAccount {
        account_id: String,
    },
    /// The QR/invite payload for one account, including its own onion.
    MessengerGetInvitePayload {
        account_id: String,
    },
    MessengerSetProfile {
        account_id: String,
        display_name: String,
        #[serde(default)]
        avatar_base64: Option<String>,
    },
    /// Local tier policy: standard (100 MiB) or Pro (2 GiB), configurable chunk.
    MessengerSetTier {
        account_id: String,
        pro: bool,
        #[serde(default)]
        chunk_size: Option<u32>,
    },

    /// Import a peer from a scanned or pasted invite payload.
    MessengerAddContact {
        account_id: String,
        /// The JSON invite, exactly as the QR encodes it.
        invite: String,
    },
    /// Pending invites that arrived from another device.
    MessengerListContactRequests {
        account_id: String,
    },
    /// Answer a request that arrived from another device. Accepting creates the
    /// contact on both sides; declining remembers nothing.
    MessengerRespondContactRequest {
        account_id: String,
        user_id: String,
        accept: bool,
    },
    MessengerListConversations {
        account_id: String,
    },
    MessengerGetMessages {
        account_id: String,
        conversation_id: String,
    },
    MessengerSendText {
        account_id: String,
        /// Peer user id for a direct conversation, group id for a group.
        to: String,
        text: String,
        #[serde(default)]
        reply_to: Option<String>,
        #[serde(default)]
        group: bool,
    },
    MessengerSendMedia {
        account_id: String,
        to: String,
        /// A single file, kept for compatibility. New interfaces send `paths`.
        #[serde(default)]
        path: Option<PathBuf>,
        /// Every file attached to this one message. Empty when `path` is used.
        #[serde(default)]
        paths: Vec<PathBuf>,
        #[serde(default)]
        caption: Option<String>,
        #[serde(default)]
        reply_to: Option<String>,
        #[serde(default)]
        group: bool,
    },
    /// Text-only in v1; media is delete-and-resend.
    MessengerEditMessage {
        account_id: String,
        id: String,
        new_text: String,
    },
    MessengerMarkRead {
        account_id: String,
        conversation_id: String,
    },

    MessengerCreateGroup {
        account_id: String,
        name: String,
        /// Contacts to include; the creator is always a member.
        member_ids: Vec<String>,
    },
    MessengerAddGroupMembers {
        account_id: String,
        group_id: String,
        member_ids: Vec<String>,
    },
    MessengerRemoveGroupMember {
        account_id: String,
        group_id: String,
        user_id: String,
    },
    MessengerListGroups {
        account_id: String,
    },
    MessengerGetGroupMessages {
        account_id: String,
        group_id: String,
    },
    /// Retry every queued message for an account, e.g. after the peer returns.
    MessengerFlushQueue {
        account_id: String,
    },

    Shutdown,
}

/// One row of `Event::SessionList`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub title: Option<String>,
    pub workspace: PathBuf,
    pub model: String,
    pub updated_at: i64,
    pub turns: i64,
    pub is_current: bool,
}

/// One visible turn replayed after `Op::ResumeSession`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryTurn {
    pub role: String,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    /// Allow, and stop asking for this exact command for the rest of the session.
    AlwaysAllow,
    Deny,
}

// ---------------------------------------------------------------------------
// Core -> interface
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    DeviceResponse {
        request_id: String,
        result: serde_json::Value,
    },
    /// A turn-scoped event tagged with the conversation that produced it.
    /// Global lifecycle/configuration events stay at the top level. This lets
    /// desktop interfaces keep several turns streaming concurrently without
    /// ever appending a token or approval card to the wrong transcript.
    SessionEvent {
        session_id: String,
        payload: Box<Event>,
    },

    /// Sent once on connect so a late-joining client can draw a full screen
    /// without replaying history.
    Ready {
        session_id: String,
        provider: String,
        model: String,
        #[serde(default = "default_reasoning_effort")]
        reasoning_effort: String,
        workspace: PathBuf,
        sandbox: String,
        web_search_enabled: bool,
        git_branch: Option<String>,
        /// Recently used workspaces, newest first, for the `/workspace` picker.
        #[serde(default)]
        recent_workspaces: Vec<String>,
        /// Available models for the current provider, for the model picker.
        #[serde(default)]
        models: Vec<String>,
        /// Raw context window (tokens) for the active provider/model, when the
        /// vendored models.dev snapshot knows it. `None` keeps the client
        /// silent rather than guessing.
        #[serde(default)]
        context_window: Option<i64>,
        #[serde(default)]
        mcp_servers: Vec<McpServerConfig>,
        #[serde(default)]
        subagent_use_separate_model: bool,
        #[serde(default = "default_inherit")]
        subagent_provider_id: String,
        #[serde(default = "default_inherit")]
        subagent_model: String,
        #[serde(default = "default_reasoning_effort")]
        subagent_reasoning_effort: String,
    },

    SessionReset,

    /// Answer to `Op::ListSessions`.
    SessionList {
        sessions: Vec<SessionSummary>,
    },

    /// Visible history of a resumed session, oldest first.
    HistoryReplay {
        turns: Vec<HistoryTurn>,
    },

    ProviderChanged {
        provider: String,
        model: String,
        /// Available models for the new provider.
        #[serde(default)]
        models: Vec<String>,
    },

    SubagentDefaultsChanged {
        enabled: bool,
        provider_id: String,
        model: String,
        #[serde(default = "default_reasoning_effort")]
        reasoning_effort: String,
    },

    /// Device-code details emitted by the OpenAI account login flow.
    ProviderLoginDeviceCode {
        provider_id: String,
        verification_url: String,
        user_code: String,
    },

    /// Completion of an account login. A successful login also selects the
    /// provider before this event is emitted.
    ProviderLoginFinished {
        provider_id: String,
        success: bool,
        message: String,
    },

    WebSearchChanged {
        enabled: bool,
    },

    McpConfigChanged {
        servers: Vec<McpServerConfig>,
    },

    WhatsAppConfigChanged {
        enabled: bool,
        assistant_name: String,
        has_own_number: bool,
        allowed_jids: Vec<String>,
    },

    NodeHubConfigChanged {
        enabled: bool,
        bind: String,
        port: u16,
    },

    TurnStarted {
        turn_id: i64,
    },

    /// One streaming delta. Keep these small and frequent; the perceived speed
    /// of the whole product is decided here.
    Token {
        text: String,
    },

    /// Reasoning-model thinking, kept separate so the UI can fold it away.
    Reasoning {
        text: String,
    },

    ToolCallStarted {
        call_id: String,
        name: String,
        /// Already summarised for display — the UI should never have to parse
        /// tool arguments to know what to print.
        summary: String,
    },

    /// Incremental output from a long-running command, so a `cargo build` is
    /// visible while it runs rather than after it finishes.
    ToolOutput {
        call_id: String,
        chunk: String,
    },

    ToolCallEnded {
        call_id: String,
        ok: bool,
        duration_ms: u64,
    },

    /// The core blocks until a matching `Op::Approve` arrives.
    ApprovalRequest {
        call_id: String,
        command: String,
        cwd: PathBuf,
        /// Why approval is needed: outside the sandbox, network access, etc.
        reason: String,
        /// Privileged commands deliberately disable session-wide approval.
        allow_always: bool,
    },

    /// The core needs a sudo credential. Interfaces must render this as a
    /// masked local input and must never add its value to the transcript.
    PrivilegeCredentialRequest {
        request_id: String,
        command: String,
        keyring_available: bool,
        attempt: u8,
        /// Exact PAM/sudo prompt when authentication uses the dynamic
        /// askpass conversation. It can be a password, OTP or another
        /// administrator challenge and is never sent to the model.
        prompt: Option<String>,
        dynamic: bool,
        message: Option<String>,
    },

    PatchApplied {
        files: Vec<PathBuf>,
        diff: String,
    },

    /// Result of one verification pass.
    Verification {
        stage: String,
        passed: bool,
        summary: String,
    },

    Compacted {
        freed_tokens: i64,
    },

    TurnCompleted {
        turn_id: i64,
        input_tokens: i64,
        output_tokens: i64,
        duration_ms: u64,
    },

    Interrupted,

    Notice {
        message: String,
    },

    // -----------------------------------------------------------------------
    // Messenger events. Payloads are JSON because the shapes are owned by the
    // messenger module; the protocol only carries them and never inspects them.
    // -----------------------------------------------------------------------
    MessengerAccountsChanged {
        accounts: Vec<serde_json::Value>,
    },
    MessengerInvite {
        account_id: String,
        payload: serde_json::Value,
    },
    MessengerConversations {
        account_id: String,
        conversations: Vec<serde_json::Value>,
    },
    /// Contacts of one account. Sent with the conversation list so the group
    /// member picker never has to guess who is available.
    MessengerContacts {
        account_id: String,
        contacts: Vec<serde_json::Value>,
    },
    /// Invites that arrived from another device and still need an answer.
    MessengerContactRequests {
        account_id: String,
        requests: Vec<serde_json::Value>,
    },
    MessengerHistory {
        account_id: String,
        conversation_id: String,
        messages: Vec<serde_json::Value>,
    },
    MessengerGroups {
        account_id: String,
        groups: Vec<serde_json::Value>,
    },
    MessengerMessageReceived {
        account_id: String,
        message: serde_json::Value,
    },
    MessengerMessageStatus {
        account_id: String,
        message_id: String,
        status: String,
        delivered_to: u32,
        total_members: u32,
    },
    MessengerMessageEdited {
        account_id: String,
        message: serde_json::Value,
    },
    MessengerPeerOnline {
        account_id: String,
        user_id: String,
    },
    MessengerPeerOffline {
        account_id: String,
        user_id: String,
    },
    MessengerMediaProgress {
        account_id: String,
        message_id: String,
        transfer_id: String,
        direction: String,
        file_name: String,
        received: u32,
        total: u32,
        status: String,
    },
    MessengerGroupMemberUpdate {
        account_id: String,
        group: serde_json::Value,
    },
    MessengerGroupMessageReceived {
        account_id: String,
        message: serde_json::Value,
    },

    Error {
        message: String,
        /// False for things the user can fix and retry — a failed patch, a
        /// rejected command. True means the session is finished.
        fatal: bool,
    },
}

fn default_reasoning_effort() -> String {
    "default".into()
}

fn default_harness2_k() -> u32 {
    2
}

fn default_harness2_mode() -> String {
    "both".into()
}

fn default_inherit() -> String {
    "inherit".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_secrets_are_redacted_from_debug_output() {
        let op = Op::SetProvider {
            provider_id: "openai".into(),
            api_key: Some(SecretString::new("sk-not-for-logs")),
            base_url: None,
        };
        let debug = format!("{op:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("sk-not-for-logs"));
    }

    #[test]
    fn provider_changed_serializes_the_live_model_list() {
        let event = Event::ProviderChanged {
            provider: "DeepSeek".into(),
            model: "deepseek-v4-pro".into(),
            models: vec!["deepseek-v4-pro".into(), "deepseek-v4-flash".into()],
        };
        let value = serde_json::to_value(event).unwrap();
        assert_eq!(value["event"], "provider_changed");
        assert_eq!(value["models"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn ready_serializes_the_context_window() {
        let event = Event::Ready {
            session_id: "s".into(),
            provider: "Ollama (local)".into(),
            model: "qwen3-coder".into(),
            reasoning_effort: "medium".into(),
            workspace: "/tmp".into(),
            sandbox: "normal".into(),
            web_search_enabled: false,
            context_window: Some(262_144),
            git_branch: None,
            recent_workspaces: Vec::new(),
            models: vec!["qwen3-coder".into()],
            mcp_servers: Vec::new(),
            subagent_use_separate_model: false,
            subagent_provider_id: "inherit".into(),
            subagent_model: "inherit".into(),
            subagent_reasoning_effort: "medium".into(),
        };
        let value = serde_json::to_value(event).unwrap();
        assert_eq!(value["context_window"].as_i64(), Some(262_144));
    }

    #[test]
    fn privilege_secrets_are_redacted_from_debug_output() {
        let op = Op::ProvidePrivilegeCredential {
            request_id: "request".into(),
            credential: Some(SecretString::new("root-secret")),
            remember: true,
        };
        let debug = format!("{op:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("root-secret"));
    }

    /// The desktop composer emits these exact payloads, tag included. They
    /// used to be dropped by the bridge as "invalid Avalonia operation" while
    /// the UI still printed "started", so the tags are pinned here by name
    /// and by shape.
    #[test]
    fn the_desktop_harness2_payloads_parse_and_keep_their_tag() {
        let plan: Op = serde_json::from_str(
            r#"{"op":"harness2_plan","task":"fix the totals","mode":"both","k":2}"#,
        )
        .expect("harness2_plan is a real operation");
        match plan {
            Op::Harness2Plan { task, k, mode } => {
                assert_eq!(
                    (task.as_str(), k, mode.as_str()),
                    ("fix the totals", 2, "both")
                );
            }
            other => panic!("harness2_plan became {other:?}"),
        }

        let start: Op = serde_json::from_str(
            r#"{"op":"harness2_start","task":"widen the tests","mode":"sequential","k":3}"#,
        )
        .expect("harness2_start is a real operation");
        match start {
            Op::Harness2Start { task, k, mode } => {
                assert_eq!(
                    (task.as_str(), k, mode.as_str()),
                    ("widen the tests", 3, "sequential")
                );
            }
            other => panic!("harness2_start became {other:?}"),
        }

        // The interactive `/harness2` op keeps its own tag and its own flag.
        let interactive: Op =
            serde_json::from_str(r#"{"op":"harness2","task":"x","dry_run":true}"#).unwrap();
        match interactive {
            Op::Harness2 {
                k, mode, dry_run, ..
            } => {
                assert_eq!((k, mode.as_str(), dry_run), (2, "both", true));
            }
            other => panic!("harness2 became {other:?}"),
        }
    }

    /// A plan request must never be able to arrive as a sweep, so the two
    /// tags stay separate variants rather than one shape with a flag the
    /// client could omit.
    #[test]
    fn a_plan_request_carries_no_way_to_start_a_sweep() {
        let plan: Op =
            serde_json::from_str(r#"{"op":"harness2_plan","task":"x","dry_run":false}"#).unwrap();
        assert!(matches!(plan, Op::Harness2Plan { .. }));
        let start: Op =
            serde_json::from_str(r#"{"op":"harness2_start","task":"x","dry_run":true}"#).unwrap();
        assert!(matches!(start, Op::Harness2Start { .. }));
    }

    #[test]
    fn turn_events_keep_their_session_route() {
        let event = Event::SessionEvent {
            session_id: "session-b".into(),
            payload: Box::new(Event::Token {
                text: "background text".into(),
            }),
        };
        let value = serde_json::to_value(event).unwrap();
        assert_eq!(value["event"], "session_event");
        assert_eq!(value["session_id"], "session-b");
        assert_eq!(value["payload"]["event"], "token");
        assert_eq!(value["payload"]["text"], "background text");
    }
}
