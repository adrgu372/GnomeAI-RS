//! The wire between the coding-agent core and every interface.
//!
//! One queue pair: interfaces send `Op`, the core emits `Event`. Nothing else
//! crosses the boundary — no shared state, no direct calls into the core, no
//! agent logic living inside an HTTP handler.
//!
//! This is the piece to build while the codebase is still small. Once the
//! agent loop has leaked into axum handlers you cannot add a TUI without
//! rewriting both, and that refactor is miserable.
//!
//! Three clients, one protocol:
//!   - the ratatui TUI      (tokio mpsc, in-process)
//!   - the web UI           (the same events, serialised over SSE)
//!   - the headless CLI     (drains events, prints, exits)

use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;
use zeroize::Zeroize;

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
    /// Start a turn with this user message.
    Submit {
        text: String,
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

    /// Select a hosted provider or one of the official account-backed CLIs.
    /// The core persists API credentials in its owner-only settings file;
    /// they are never echoed back in an event.
    SetProvider {
        provider_id: String,
        api_key: Option<SecretString>,
        base_url: Option<String>,
    },

    /// Enable or disable all web-search and web-fetch tools. The setting is
    /// persisted outside the conversation transcript.
    SetWebSearch {
        enabled: bool,
    },

    /// Change the sandbox policy mid-session. Narrowing takes effect on the
    /// next command; widening should require the user to confirm.
    SetSandbox {
        mode: String,
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

    /// Run environment diagnostics and report the results as a notice.
    Doctor,

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
    /// Sent once on connect so a late-joining client can draw a full screen
    /// without replaying history.
    Ready {
        session_id: String,
        provider: String,
        model: String,
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

    WebSearchChanged {
        enabled: bool,
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

    Error {
        message: String,
        /// False for things the user can fix and retry — a failed patch, a
        /// rejected command. True means the session is finished.
        fatal: bool,
    },
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
}
