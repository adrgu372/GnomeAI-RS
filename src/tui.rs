//! Coding-agent terminal UI — ratatui.
//!
//! Immediate mode: every frame is redrawn from `App` state. There is no
//! component tree and no lifecycle, so the whole design question is "what does
//! this screen show given this state", and the answer lives in `draw`.
//!
//! The properties worth getting right, in order of how much they matter:
//!
//!   1. Input never blocks. You can type, scroll and queue messages while the
//!      agent is mid-tool-call. A TUI that freezes during work is the single
//!      most common way these things feel broken.
//!   2. Interrupt works when busy — the only moment anyone presses it.
//!   3. The composer stays pinned to the bottom; the transcript scrolls behind
//!      it. Cursor never moves out from under your hands.
//!   4. Messages typed while busy are queued, not lost.

use anyhow::Result;
use base64::Engine as _;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event as TermEvent, EventStream, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use futures_util::StreamExt;
use ratatui::prelude::*;
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Wrap,
};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use zeroize::Zeroize;

use crate::protocol::{Decision, Event, Op, SecretString, SessionSummary};
use crate::provider_catalog::{AuthKind, PROVIDERS};

// ---------------------------------------------------------------------------
// Transcript
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum Block_ {
    User(String),
    Assistant(String),
    Reasoning(String),
    Tool {
        name: String,
        summary: String,
        output: String,
        done: bool,
        ok: bool,
        ms: u64,
    },
    Diff(String),
    Verify {
        stage: String,
        passed: bool,
        summary: String,
    },
    Error(String),
    Note(String),
}

#[derive(Debug, Clone)]
struct PendingImage {
    path: PathBuf,
    media_type: String,
}

/// Slash commands the TUI owns. Anything unrecognised is sent to the core, so
/// the agent side can add commands without a TUI release.
const COMMANDS: &[(&str, &str)] = &[
    ("/help", "show every command with what it does"),
    ("/new", "start a fresh session"),
    ("/sessions", "list, resume, rename or delete saved sessions"),
    ("/resume", "resume a session: /resume ID"),
    ("/fork", "branch the current session at its tip"),
    ("/compact", "compress context now"),
    ("/rollback", "undo every patch in this session"),
    ("/workspace", "switch project directory; bare = recent list"),
    ("/cd", "alias for /workspace"),
    ("/provider", "choose API provider or account login"),
    ("/model", "switch model"),
    ("/websearch", "toggle web search on/off"),
    ("/sandbox", "change sandbox policy"),
    (
        "/skills",
        "list installed Agent Skills compatible with SKILL.md",
    ),
    ("/skill", "use/install/update/inspect/verify/remove a skill"),
    (
        "/memory",
        "shared memory; /memory status|show|dream [--dry-run]|reindex|forget ID|clear|on|off",
    ),
    ("/mouse", "toggle app mouse support: /mouse on|off"),
    ("/copy", "copy the selection or last assistant reply"),
    (
        "/contrast",
        "toggle high-contrast colors (light terminal backgrounds)",
    ),
    ("/tokens", "show per-turn token usage and session totals"),
    ("/notify", "toggle desktop notifications: /notify on|off"),
    ("/doctor", "diagnose configuration and environment"),
    ("/diff", "show the accumulated diff"),
    ("/export", "export conversation to markdown file"),
    ("/clear", "clear the transcript (history is preserved)"),
    ("/quit", "exit"),
];

/// These commands own searchable pickers, so the generic inline suggestion
/// must not complete them. They remain visible in the `/` command browser and
/// still open their dedicated dialogs when submitted exactly.
const DEDICATED_PICKER_COMMANDS: &[&str] = &["/provider", "/model"];

#[derive(Debug)]
enum ProviderStage {
    Select,
    BaseUrl,
    ApiKey,
}

#[derive(Debug)]
struct ProviderDialog {
    selected: usize,
    stage: ProviderStage,
    input: String,
    base_url: Option<String>,
    error: Option<String>,
}

impl ProviderDialog {
    fn new() -> Self {
        Self {
            selected: 0,
            stage: ProviderStage::Select,
            input: String::new(),
            base_url: None,
            error: None,
        }
    }
}

#[derive(Debug)]
struct ModelDialog {
    models: Vec<String>,
    selected: usize,
    query: String,
}

impl ModelDialog {
    fn new(models: Vec<String>, current: &str) -> Self {
        let selected = models
            .iter()
            .position(|model| model == current)
            .unwrap_or(0);
        Self {
            models,
            selected,
            query: String::new(),
        }
    }

    fn matches(&self) -> Vec<&str> {
        let query = self.query.trim().to_lowercase();
        self.models
            .iter()
            .filter(|model| query.is_empty() || model.to_lowercase().contains(&query))
            .map(String::as_str)
            .collect()
    }

    fn selected_model(&self) -> Option<String> {
        self.matches()
            .get(self.selected)
            .map(|model| model.to_string())
    }

    fn clamp_selection(&mut self) {
        let maximum = self.matches().len().saturating_sub(1);
        self.selected = self.selected.min(maximum);
    }
}

enum UiAction {
    None,
    Authenticate {
        provider_id: String,
        flow: AccountLogin,
    },
}

#[derive(Debug)]
struct SessionDialog {
    sessions: Vec<SessionSummary>,
    selected: usize,
    /// `Some` while typing a new title for the selected session.
    renaming: Option<String>,
}

struct PrivilegeDialog {
    request_id: String,
    command: String,
    input: String,
    remember: bool,
    keyring_available: bool,
    attempt: u8,
    message: Option<String>,
}

impl Drop for PrivilegeDialog {
    fn drop(&mut self) {
        self.input.zeroize();
    }
}

enum AccountLogin {
    Codex,
    ClaudeCode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct TextPoint {
    row: u16,
    column: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TextSelection {
    anchor: TextPoint,
    focus: TextPoint,
    dragging: bool,
}

impl TextSelection {
    fn normalized(self) -> (TextPoint, TextPoint) {
        if self.anchor <= self.focus {
            (self.anchor, self.focus)
        } else {
            (self.focus, self.anchor)
        }
    }

    fn is_empty(self) -> bool {
        self.anchor == self.focus
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

pub struct App {
    blocks: Vec<Block_>,
    composer: String,
    cursor: usize,
    /// Typed while the agent was busy. Sent in order when it goes idle.
    queue: VecDeque<String>,
    history: Vec<String>,
    history_pos: Option<usize>,

    busy: bool,
    turn_started: Option<Instant>,
    /// Top-based transcript row currently shown by the renderer.
    scroll: u16,
    /// Last rendered maximum scroll offset, used by keyboard and mouse input.
    max_scroll: u16,
    /// True while the user has scrolled up; suppresses auto-follow so the view
    /// does not yank away mid-read.
    detached: bool,

    completions: Vec<&'static str>,
    completion_idx: usize,
    /// First completion row currently drawn. Kept in step with
    /// `completion_idx` so the highlight can never scroll out of view.
    completion_offset: usize,

    pending_approval: Option<(String, String, bool)>,
    privilege_dialog: Option<PrivilegeDialog>,
    provider_dialog: Option<ProviderDialog>,
    model_dialog: Option<ModelDialog>,
    session_dialog: Option<SessionDialog>,

    provider: String,
    model: String,
    models: Vec<String>,
    workspace: String,
    branch: Option<String>,
    sandbox: String,
    web_search_enabled: bool,
    recent_workspaces: Vec<String>,
    tokens_in: i64,
    tokens_out: i64,

    /// Mouse capture state. Off means the terminal's native selection and
    /// clipboard work without holding Shift; the wheel stops scrolling us.
    mouse_enabled: bool,
    /// Set by `/mouse`; the run loop owns the terminal and applies it.
    mouse_toggle: Option<bool>,
    /// Screen rectangle occupied by the transcript in the most recent frame.
    /// Mouse coordinates are projected through this rectangle into absolute
    /// wrapped transcript rows.
    transcript_area: Rect,
    /// App-owned selection. Keeping this independent from the terminal's
    /// native selection lets the wheel continue scrolling while dragging.
    selection: Option<TextSelection>,
    /// Short-lived clipboard feedback shown in the status bar. This must not
    /// be appended to the transcript because doing so would move the selected
    /// rows immediately after mouse-up.
    clipboard_notice: Option<(String, Instant)>,
    /// Images captured by Ctrl+V and therefore trusted for outbound upload.
    /// User-typed `[image: ...]` markers never grant arbitrary file access.
    pending_images: HashMap<String, PendingImage>,

    should_quit: bool,

    // -----------------------------------------------------------------------
    // New features
    // -----------------------------------------------------------------------
    /// Search mode state
    search_mode: bool,
    search_query: String,
    search_matches: Vec<usize>, // indices into blocks
    search_idx: usize,
    /// Multi-line composer: true = newline inserted, false = submit
    composer_multiline: bool,
    /// High contrast mode for light terminal backgrounds
    high_contrast: bool,
    /// Per-turn token usage history: (input_tokens, output_tokens, duration_ms)
    token_history: Vec<(i64, i64, u64)>,
    /// Desktop notifications on turn completion and errors.
    notifications_enabled: bool,
}

impl App {
    pub fn new() -> Self {
        Self {
            blocks: Vec::new(),
            composer: String::new(),
            cursor: 0,
            queue: VecDeque::new(),
            history: Vec::new(),
            history_pos: None,
            busy: false,
            turn_started: None,
            scroll: 0,
            max_scroll: 0,
            detached: false,
            completions: Vec::new(),
            completion_idx: 0,
            completion_offset: 0,
            pending_approval: None,
            privilege_dialog: None,
            provider_dialog: None,
            model_dialog: None,
            session_dialog: None,
            provider: "—".into(),
            model: "—".into(),
            models: Vec::new(),
            workspace: "—".into(),
            branch: None,
            sandbox: "normal".into(),
            web_search_enabled: false,
            recent_workspaces: Vec::new(),
            tokens_in: 0,
            tokens_out: 0,
            mouse_enabled: true,
            mouse_toggle: None,
            transcript_area: Rect::default(),
            selection: None,
            clipboard_notice: None,
            pending_images: HashMap::new(),
            should_quit: false,
            search_mode: false,
            search_query: String::new(),
            search_matches: Vec::new(),
            search_idx: 0,
            composer_multiline: false,
            high_contrast: false,
            token_history: Vec::new(),
            notifications_enabled: true,
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        for image in self.pending_images.values() {
            let _ = std::fs::remove_file(&image.path);
        }
    }
}

// ---------------------------------------------------------------------------
// Run loop
// ---------------------------------------------------------------------------

pub async fn run(ops: mpsc::Sender<Op>, mut events: mpsc::Receiver<Event>) -> Result<()> {
    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste);
    let mut app = App::new();
    let mut input = EventStream::new();
    // Drives the stopwatch and the spinner. Nothing else needs a timer.
    let mut tick = tokio::time::interval(Duration::from_millis(100));

    let res = loop {
        if let Err(error) = terminal.draw(|f| draw(f, &mut app)) {
            break Err(error.into());
        }

        tokio::select! {
            // Core events and terminal input are peers here. That is the whole
            // trick behind a non-blocking UI: neither can starve the other.
            ev = events.recv() => {
                match ev {
                    Some(ev) => apply_event(&mut app, ev),
                    None => break Ok(()),
                }
            }
            Some(Ok(term)) = input.next() => {
                match term {
                    TermEvent::Key(key) if key.is_press() => {
                        match handle_key(&mut app, key, &ops).await {
                            UiAction::None => {}
                            UiAction::Authenticate {
                                provider_id,
                                flow,
                            } => {
                                let _ =
                                    crossterm::execute!(
                                        std::io::stdout(),
                                        DisableMouseCapture,
                                        DisableBracketedPaste
                                    );
                                ratatui::restore();
                                let login = run_account_login(flow).await;
                                terminal = ratatui::init();
                                let _ =
                                    crossterm::execute!(std::io::stdout(), EnableBracketedPaste);
                                if app.mouse_enabled {
                                    let _ =
                                        crossterm::execute!(std::io::stdout(), EnableMouseCapture);
                                }
                                input = EventStream::new();

                                match login {
                                    Ok(()) => {
                                        let _ = ops
                                            .send(Op::SetProvider {
                                                provider_id,
                                                api_key: None,
                                                base_url: None,
                                            })
                                            .await;
                                    }
                                    Err(error) => app.blocks.push(Block_::Error(error.to_string())),
                                }
                            }
                        }
                    }
                    TermEvent::Paste(text) => {
                        if let Some(dialog) = app.privilege_dialog.as_mut() {
                            dialog
                                .input
                                .extend(text.chars().filter(|character| !matches!(character, '\r' | '\n' | '\0')));
                        } else {
                            handle_paste(&mut app, &text);
                        }
                    }
                    TermEvent::Mouse(mouse) => handle_mouse(&mut app, mouse),
                    _ => {}
                }
            }
            _ = tick.tick() => {}
            else => break Ok(()),
        }

        // Auto-dismiss clipboard notice after 3 seconds (frees the Option)
        if app
            .clipboard_notice
            .as_ref()
            .is_some_and(|(_, created)| created.elapsed() >= Duration::from_secs(3))
        {
            app.clipboard_notice = None;
        }

        // `/mouse` toggles capture. Only the run loop touches the terminal, so
        // the request travels through App state instead of a direct call.
        if let Some(enable) = app.mouse_toggle.take() {
            if enable != app.mouse_enabled {
                let result = if enable {
                    crossterm::execute!(std::io::stdout(), EnableMouseCapture)
                } else {
                    crossterm::execute!(std::io::stdout(), DisableMouseCapture)
                };
                match result {
                    Ok(()) => {
                        app.mouse_enabled = enable;
                        app.blocks.push(Block_::Note(if enable {
                            "mouse capture on — drag to select, keep dragging and use the wheel \
                             to extend, release to copy"
                                .into()
                        } else {
                            "mouse capture off — select and copy text normally; scroll with \
                             PageUp/PageDown or Ctrl+↑/↓"
                                .into()
                        }));
                    }
                    Err(error) => app.blocks.push(Block_::Error(error.to_string())),
                }
            }
        }

        if app.should_quit {
            break Ok(());
        }

        // Drain the queue the moment the agent frees up.
        if !app.busy && app.pending_approval.is_none() {
            if let Some(next) = app.queue.pop_front() {
                submit(&mut app, next, &ops).await;
            }
        }
    };

    let _ = ops.send(Op::Shutdown).await;
    let _ = crossterm::execute!(
        std::io::stdout(),
        DisableMouseCapture,
        DisableBracketedPaste
    );
    ratatui::restore();
    res
}

async fn run_account_login(flow: AccountLogin) -> Result<()> {
    match flow {
        AccountLogin::Codex => {
            println!("\nGnomeAI-RS paused its UI for the official OpenAI sign-in flow.\n");
            crate::codex_app_server::login_with_chatgpt().await
        }
        AccountLogin::ClaudeCode => crate::provider::login_with_claude().await,
    }
}

// ---------------------------------------------------------------------------
// Events from the core
// ---------------------------------------------------------------------------

fn apply_event(app: &mut App, ev: Event) {
    match ev {
        Event::Ready {
            provider,
            model,
            workspace,
            sandbox,
            web_search_enabled,
            git_branch,
            recent_workspaces,
            models,
            ..
        } => {
            app.provider = provider;
            app.model = model;
            set_available_models(app, models);
            app.workspace = workspace.display().to_string();
            app.sandbox = sandbox;
            app.web_search_enabled = web_search_enabled;
            app.branch = git_branch;
            app.recent_workspaces = recent_workspaces;
        }

        Event::SessionList { sessions } => match app.session_dialog.as_mut() {
            Some(dialog) => {
                dialog.selected = dialog.selected.min(sessions.len().saturating_sub(1));
                dialog.sessions = sessions;
                dialog.renaming = None;
            }
            None => {
                app.session_dialog = Some(SessionDialog {
                    sessions,
                    selected: 0,
                    renaming: None,
                });
            }
        },

        Event::HistoryReplay { turns } => {
            for turn in turns {
                match turn.role.as_str() {
                    "user" => app.blocks.push(Block_::User(turn.text)),
                    "assistant" => app.blocks.push(Block_::Assistant(turn.text)),
                    _ => app.blocks.push(Block_::Note(turn.text)),
                }
            }
        }

        Event::SessionReset => {
            app.blocks.clear();
            app.queue.clear();
            app.busy = false;
            app.turn_started = None;
            app.scroll = 0;
            app.max_scroll = 0;
            app.detached = false;
            app.selection = None;
            app.tokens_in = 0;
            app.tokens_out = 0;
            app.token_history.clear();
        }

        Event::ProviderChanged {
            provider,
            model,
            models,
        } => {
            app.provider = provider;
            app.model = model;
            set_available_models(app, models);
        }

        Event::WebSearchChanged { enabled } => {
            app.web_search_enabled = enabled;
        }

        Event::TurnStarted { .. } => {
            app.busy = true;
            app.turn_started = Some(Instant::now());
        }

        // Append into the trailing assistant block rather than pushing a new
        // one per token, or the transcript becomes a million-element vector
        // and rendering gets quadratic.
        Event::Token { text } => match app.blocks.last_mut() {
            Some(Block_::Assistant(buf)) => buf.push_str(&text),
            _ => app.blocks.push(Block_::Assistant(text)),
        },

        Event::Reasoning { text } => match app.blocks.last_mut() {
            Some(Block_::Reasoning(buf)) => buf.push_str(&text),
            _ => app.blocks.push(Block_::Reasoning(text)),
        },

        Event::ToolCallStarted { name, summary, .. } => {
            app.blocks.push(Block_::Tool {
                name,
                summary,
                output: String::new(),
                done: false,
                ok: false,
                ms: 0,
            });
        }

        Event::ToolOutput { chunk, .. } => {
            if let Some(Block_::Tool { output, .. }) = app.blocks.last_mut() {
                output.push_str(&chunk);
                // Keep only the tail on screen; the full text lives in SQLite.
                if output.len() > 4096 {
                    let mut cut = output.len() - 4096;
                    while !output.is_char_boundary(cut) {
                        cut += 1;
                    }
                    *output = output[cut..].to_string();
                }
            }
        }

        Event::ToolCallEnded {
            ok: success,
            duration_ms,
            ..
        } => {
            if let Some(Block_::Tool { done, ok, ms, .. }) = app.blocks.last_mut() {
                *done = true;
                *ok = success;
                *ms = duration_ms;
            }
        }

        Event::ApprovalRequest {
            call_id,
            command,
            reason,
            allow_always,
            ..
        } => {
            if app.notifications_enabled {
                let short: String = command.chars().take(120).collect();
                desktop_notify("GnomeAI — approval needed", &format!("{short}\n{reason}"));
            }
            app.pending_approval = Some((call_id, format!("{command}\n\n{reason}"), allow_always));
        }

        Event::PrivilegeCredentialRequest {
            request_id,
            command,
            keyring_available,
            attempt,
            message,
        } => {
            if app.notifications_enabled {
                desktop_notify("GnomeAI — sudo authentication", &command);
            }
            app.privilege_dialog = Some(PrivilegeDialog {
                request_id,
                command,
                input: String::with_capacity(128),
                remember: false,
                keyring_available,
                attempt,
                message,
            });
        }

        Event::PatchApplied { diff, .. } => app.blocks.push(Block_::Diff(diff)),

        Event::Verification {
            stage,
            passed,
            summary,
        } => app.blocks.push(Block_::Verify {
            stage,
            passed,
            summary,
        }),

        Event::Compacted { freed_tokens } => app.blocks.push(Block_::Note(format!(
            "context compacted, {freed_tokens} tokens freed"
        ))),

        Event::TurnCompleted {
            input_tokens,
            output_tokens,
            duration_ms,
            ..
        } => {
            app.busy = false;
            app.turn_started = None;
            app.tokens_in += input_tokens;
            app.tokens_out += output_tokens;
            app.token_history
                .push((input_tokens, output_tokens, duration_ms));
            if app.notifications_enabled {
                desktop_notify(
                    "GnomeAI — turn complete",
                    &format!(
                        "{} tokens out · {:.1}s · {}",
                        output_tokens,
                        duration_ms as f64 / 1000.0,
                        app.model,
                    ),
                );
            }
        }

        Event::Interrupted => {
            app.busy = false;
            app.turn_started = None;
            app.blocks.push(Block_::Note("interrupted".into()));
        }

        Event::Notice { message } => app.blocks.push(Block_::Note(message)),

        Event::Error { message, fatal } => {
            app.busy = false;
            if fatal {
                if app.notifications_enabled {
                    desktop_notify("GnomeAI — fatal error", &message);
                }
                app.should_quit = true;
            }
            app.blocks.push(Block_::Error(message));
        }
    }
}

fn set_available_models(app: &mut App, mut models: Vec<String>) {
    models = models
        .into_iter()
        .map(|model| model.trim().to_string())
        .filter(|model| !model.is_empty())
        .collect();
    models.sort_unstable();
    models.dedup();

    let current = app.model.trim();
    if !current.is_empty() && current != "—" {
        if let Some(index) = models.iter().position(|model| model == current) {
            models.remove(index);
        }
        models.insert(0, current.to_string());
    }
    app.models = models;
}

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

async fn handle_key(app: &mut App, key: KeyEvent, ops: &mpsc::Sender<Op>) -> UiAction {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    // An open approval prompt swallows everything. Answering it is the only
    // thing that unblocks the core, so do not let a stray keystroke land in
    // the composer instead.
    if let Some((call_id, _, allow_always)) = app.pending_approval.clone() {
        let decision = match key.code {
            KeyCode::Char('y') => Some(Decision::Allow),
            KeyCode::Char('a') if allow_always => Some(Decision::AlwaysAllow),
            KeyCode::Char('n') | KeyCode::Esc => Some(Decision::Deny),
            _ => None,
        };
        if let Some(decision) = decision {
            app.pending_approval = None;
            let _ = ops.send(Op::Approve { call_id, decision }).await;
        }
        return UiAction::None;
    }

    if app.privilege_dialog.is_some() {
        let mut reply = None;
        let mut close = false;
        if let Some(dialog) = app.privilege_dialog.as_mut() {
            match key.code {
                KeyCode::Esc => {
                    reply = Some(Op::ProvidePrivilegeCredential {
                        request_id: dialog.request_id.clone(),
                        credential: None,
                        remember: false,
                    });
                    close = true;
                }
                KeyCode::Enter if !dialog.input.is_empty() => {
                    let secret = SecretString::new(std::mem::take(&mut dialog.input));
                    reply = Some(Op::ProvidePrivilegeCredential {
                        request_id: dialog.request_id.clone(),
                        credential: Some(secret),
                        remember: dialog.remember && dialog.keyring_available,
                    });
                    close = true;
                }
                KeyCode::Tab if dialog.keyring_available => {
                    dialog.remember = !dialog.remember;
                }
                KeyCode::Backspace => {
                    dialog.input.pop();
                }
                KeyCode::Char(character) if !ctrl && !alt => {
                    dialog.input.push(character);
                }
                _ => {}
            }
        }
        if close {
            app.privilege_dialog = None;
        }
        if let Some(reply) = reply {
            let _ = ops.send(reply).await;
        }
        return UiAction::None;
    }

    if app.provider_dialog.is_some() {
        return handle_provider_key(app, key, ops).await;
    }
    if app.model_dialog.is_some() {
        handle_model_key(app, key, ops).await;
        return UiAction::None;
    }
    if app.session_dialog.is_some() {
        handle_session_key(app, key, ops).await;
        return UiAction::None;
    }

    // Search mode handling
    if app.search_mode {
        match key.code {
            KeyCode::Esc => {
                app.search_mode = false;
                app.search_query.clear();
                app.search_matches.clear();
            }
            KeyCode::Enter => {
                // Jump to next match
                if !app.search_matches.is_empty() {
                    app.search_idx = (app.search_idx + 1) % app.search_matches.len();
                    scroll_to_match(app);
                }
            }
            KeyCode::BackTab => {
                // Jump to previous match (Shift+Tab)
                if !app.search_matches.is_empty() {
                    app.search_idx = if app.search_idx == 0 {
                        app.search_matches.len() - 1
                    } else {
                        app.search_idx - 1
                    };
                    scroll_to_match(app);
                }
            }
            KeyCode::Backspace => {
                app.search_query.pop();
                update_search_matches(app);
            }
            KeyCode::Char(c) => {
                app.search_query.push(c);
                update_search_matches(app);
            }
            _ => {}
        }
        return UiAction::None;
    }

    // Some terminals forward Ctrl+Shift+C to alternate-screen applications
    // instead of handling it themselves. Never interpret that familiar copy
    // shortcut as Ctrl+C (interrupt/quit).
    if ctrl && shift && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C')) {
        copy_selection_or_last_assistant(app);
        return UiAction::None;
    }

    match (key.code, ctrl, alt) {
        // Keyboard copy prefers the current app-owned selection, then falls
        // back to the last assistant reply.
        (KeyCode::Char('y'), true, _) => copy_selection_or_last_assistant(app),

        // Interrupt while busy, quit while idle. Same key, because that is
        // what the muscle memory expects.
        (KeyCode::Char('c'), true, _) => {
            if app.busy {
                let _ = ops.send(Op::Interrupt).await;
            } else {
                app.should_quit = true;
            }
        }

        // Ctrl+F: enter search mode
        (KeyCode::Char('f'), true, _) => {
            app.search_mode = true;
            app.search_query.clear();
            app.search_matches.clear();
            app.search_idx = 0;
        }

        // Ctrl+V: paste an image from the system clipboard into the composer
        // as an [image: PATH] marker. Text paste already arrives via the
        // terminal's bracketed-paste, so this only handles the image case.
        (KeyCode::Char('v'), true, _) => match clipboard_image().await {
            Ok(Some(image)) => {
                let key = image.path.display().to_string();
                let marker = format!("[image: {key}]");
                for ch in marker.chars() {
                    insert(app, ch);
                }
                app.pending_images.insert(key, image);
                app.clipboard_notice = Some(("image pasted from clipboard".into(), Instant::now()));
            }
            Ok(None) => {
                app.clipboard_notice = Some((
                    "no image in clipboard (text pastes via Ctrl+Shift+V)".into(),
                    Instant::now(),
                ));
            }
            Err(error) => {
                app.blocks.push(Block_::Error(error));
            }
        },

        (KeyCode::Enter, false, false) => {
            let selected_completion = app.completions.get(app.completion_idx).copied();
            let completion_is_exact =
                selected_completion.is_some_and(|name| app.composer.trim() == name);

            if app.completions.is_empty() || completion_is_exact {
                app.completions.clear();
                let text = std::mem::take(&mut app.composer);
                app.cursor = 0;
                if !text.trim().is_empty() {
                    app.history.push(text.clone());
                    app.history_pos = None;
                    if app.busy {
                        // Queue rather than drop. Losing a message you typed is
                        // unforgivable in a tool you live in.
                        app.queue.push_back(text);
                    } else {
                        submit(app, text, ops).await;
                    }
                }
            } else {
                accept_completion(app);
            }
        }

        // Shift+Enter: insert newline for multi-line composer
        (KeyCode::Enter, false, true) => {
            insert(app, '\n');
        }

        // Shift+Enter is not distinguishable on many terminals, so Alt+Enter
        // is the reliable newline.
        (KeyCode::Enter, _, true) => insert(app, '\n'),

        (KeyCode::Tab, _, _) => {
            if command_autosuggestion(app).is_some() {
                accept_completion(app);
            } else if app.completions.is_empty() {
                recompute_completions(app);
                sync_completion_offset(app);
            } else {
                move_completion(app, 1);
            }
        }
        (KeyCode::BackTab, _, _) => move_completion(app, -1),

        (KeyCode::Esc, _, _) => {
            let had_menu = !app.completions.is_empty();
            app.completions.clear();
            app.completion_offset = 0;
            // Esc closes the menu first; a second press clears the selection
            // or returns to the newest message.
            if !had_menu && app.selection.take().is_none() {
                scroll_to_bottom(app);
            }
        }

        (KeyCode::Backspace, _, _) => {
            if app.cursor > 0 {
                let prev = prev_boundary(&app.composer, app.cursor);
                app.composer.replace_range(prev..app.cursor, "");
                app.cursor = prev;
                recompute_completions(app);

                // Clear completions if composer is now empty
                if app.composer.is_empty() {
                    app.completions.clear();
                }
            }
        }

        (KeyCode::Left, _, _) => app.cursor = prev_boundary(&app.composer, app.cursor),
        (KeyCode::Right, _, _) => app.cursor = next_boundary(&app.composer, app.cursor),
        (KeyCode::Home, true, _) => scroll_to_top(app),
        (KeyCode::End, true, _) => scroll_to_bottom(app),
        (KeyCode::Home, _, _) => app.cursor = 0,
        (KeyCode::End, _, _) => app.cursor = app.composer.len(),

        (KeyCode::PageUp, _, _) => scroll_up(app, 10),
        (KeyCode::PageDown, _, _) => scroll_down(app, 10),
        (KeyCode::Up, true, _) => scroll_up(app, 3),
        (KeyCode::Down, true, _) => scroll_down(app, 3),

        // With the menu open the arrows belong to it; history and scrolling
        // stay available the moment it closes.
        (KeyCode::Up, _, _) if !app.completions.is_empty() => move_completion(app, -1),
        (KeyCode::Down, _, _) if !app.completions.is_empty() => move_completion(app, 1),

        // History only when there is nothing to lose in the composer.
        (KeyCode::Up, _, _) if app.composer.is_empty() => {
            let idx = match app.history_pos {
                Some(0) | None if app.history.is_empty() => return UiAction::None,
                Some(i) => i.saturating_sub(1),
                None => app.history.len() - 1,
            };
            app.history_pos = Some(idx);
            app.composer = app.history[idx].clone();
            app.cursor = app.composer.len();
        }
        (KeyCode::Down, _, _) if app.history_pos.is_some() => {
            let i = app.history_pos.unwrap() + 1;
            if i >= app.history.len() {
                app.history_pos = None;
                app.composer.clear();
            } else {
                app.history_pos = Some(i);
                app.composer = app.history[i].clone();
            }
            app.cursor = app.composer.len();
        }

        (KeyCode::Char(c), false, false) => insert(app, c),

        _ => {}
    }

    UiAction::None
}

fn handle_mouse(app: &mut App, mouse: MouseEvent) {
    if app.pending_approval.is_some()
        || app.privilege_dialog.is_some()
        || app.provider_dialog.is_some()
        || app.model_dialog.is_some()
        || app.session_dialog.is_some()
    {
        return;
    }
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if let Some(point) = transcript_point(app, mouse.column, mouse.row) {
                app.selection = Some(TextSelection {
                    anchor: point,
                    focus: point,
                    dragging: true,
                });
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            update_selection_focus(app, mouse.column, mouse.row);
        }
        MouseEventKind::Up(MouseButton::Left) => {
            update_selection_focus(app, mouse.column, mouse.row);
            if let Some(selection) = app.selection.as_mut() {
                selection.dragging = false;
            }
            if app.selection.is_some_and(TextSelection::is_empty) {
                app.selection = None;
            } else {
                copy_current_selection(app);
            }
        }
        MouseEventKind::ScrollUp => {
            scroll_up(app, 3);
            if app.selection.is_some_and(|selection| selection.dragging) {
                update_selection_focus(app, mouse.column, mouse.row);
            }
        }
        MouseEventKind::ScrollDown => {
            scroll_down(app, 3);
            if app.selection.is_some_and(|selection| selection.dragging) {
                update_selection_focus(app, mouse.column, mouse.row);
            }
        }
        _ => {}
    }
}

fn handle_paste(app: &mut App, text: &str) {
    const MAX_PASTE_BYTES: usize = 1024 * 1024;
    let mut text = text.replace("\r\n", "\n").replace('\r', "\n");
    if text.len() > MAX_PASTE_BYTES {
        let mut boundary = MAX_PASTE_BYTES;
        while !text.is_char_boundary(boundary) {
            boundary -= 1;
        }
        text.truncate(boundary);
        app.clipboard_notice = Some(("paste truncated to 1 MiB".into(), Instant::now()));
    }
    if let Some(dialog) = app.provider_dialog.as_mut() {
        dialog.input.push_str(text.trim());
        return;
    }
    if let Some(dialog) = app.model_dialog.as_mut() {
        dialog.query.push_str(text.trim());
        dialog.selected = 0;
        return;
    }
    if let Some(dialog) = app.session_dialog.as_mut()
        && let Some(rename) = dialog.renaming.as_mut()
    {
        rename.push_str(text.trim());
        return;
    }
    app.composer.insert_str(app.cursor, &text);
    app.cursor += text.len();
    recompute_completions(app);
}

fn transcript_point(app: &App, column: u16, row: u16) -> Option<TextPoint> {
    let area = app.transcript_area;
    let right = area.x.saturating_add(area.width);
    let bottom = area.y.saturating_add(area.height);
    if area.width == 0
        || area.height == 0
        || column < area.x
        || column >= right
        || row < area.y
        || row >= bottom
    {
        return None;
    }
    Some(TextPoint {
        row: app.scroll.saturating_add(row.saturating_sub(area.y)),
        column: column.saturating_sub(area.x),
    })
}

fn update_selection_focus(app: &mut App, column: u16, row: u16) {
    let Some(point) = transcript_point(app, column, row) else {
        return;
    };
    if let Some(selection) = app.selection.as_mut() {
        selection.focus = point;
    }
}

fn scroll_up(app: &mut App, rows: u16) {
    if !app.detached {
        app.scroll = app.max_scroll;
    }
    app.scroll = app.scroll.saturating_sub(rows);
    app.detached = app.scroll < app.max_scroll;
}

fn scroll_down(app: &mut App, rows: u16) {
    app.scroll = app.scroll.saturating_add(rows).min(app.max_scroll);
    app.detached = app.scroll < app.max_scroll;
}

fn scroll_to_top(app: &mut App) {
    app.scroll = 0;
    app.detached = app.max_scroll > 0;
}

fn scroll_to_bottom(app: &mut App) {
    app.scroll = app.max_scroll;
    app.detached = false;
}

async fn handle_provider_key(app: &mut App, key: KeyEvent, ops: &mpsc::Sender<Op>) -> UiAction {
    if key.code == KeyCode::Esc {
        app.provider_dialog = None;
        return UiAction::None;
    }

    let Some(dialog) = app.provider_dialog.as_mut() else {
        return UiAction::None;
    };
    dialog.error = None;

    match dialog.stage {
        ProviderStage::Select => match key.code {
            KeyCode::Up => {
                dialog.selected = dialog.selected.saturating_sub(1);
            }
            KeyCode::Down => {
                dialog.selected = (dialog.selected + 1).min(PROVIDERS.len() - 1);
            }
            KeyCode::Home => dialog.selected = 0,
            KeyCode::End => dialog.selected = PROVIDERS.len() - 1,
            KeyCode::Enter => {
                let selected = &PROVIDERS[dialog.selected];
                match selected.auth {
                    AuthKind::Account => {
                        let provider_id = selected.id.to_string();
                        let flow = match selected.id {
                            "openai-account" => AccountLogin::Codex,
                            "anthropic-account" => AccountLogin::ClaudeCode,
                            _ => {
                                dialog.error = Some("account login is not configured".to_string());
                                return UiAction::None;
                            }
                        };
                        app.provider_dialog = None;
                        return UiAction::Authenticate { provider_id, flow };
                    }
                    AuthKind::OptionalApiKey => {
                        dialog.stage = ProviderStage::BaseUrl;
                        dialog.input = selected.base_url.to_string();
                    }
                    AuthKind::ApiKey => {
                        dialog.stage = ProviderStage::ApiKey;
                        dialog.input.clear();
                    }
                }
            }
            _ => {}
        },
        ProviderStage::BaseUrl => match key.code {
            KeyCode::Backspace => {
                dialog.input.pop();
            }
            KeyCode::Enter => {
                let value = dialog.input.trim().trim_end_matches('/').to_string();
                if value.starts_with("http://") || value.starts_with("https://") {
                    dialog.base_url = Some(value);
                    dialog.input.clear();
                    dialog.stage = ProviderStage::ApiKey;
                } else {
                    dialog.error = Some("endpoint must start with http:// or https://".to_string());
                }
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                dialog.input.push(character);
            }
            _ => {}
        },
        ProviderStage::ApiKey => match key.code {
            KeyCode::Backspace => {
                dialog.input.pop();
            }
            KeyCode::Enter => {
                let selected = &PROVIDERS[dialog.selected];
                let key = dialog.input.trim().to_string();
                let provider_id = selected.id.to_string();
                let base_url = dialog.base_url.clone();
                let api_key = (!key.is_empty()).then(|| SecretString::new(key));
                app.provider_dialog = None;
                let _ = ops
                    .send(Op::SetProvider {
                        provider_id,
                        api_key,
                        base_url,
                    })
                    .await;
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                dialog.input.push(character);
            }
            _ => {}
        },
    }

    UiAction::None
}

async fn handle_model_key(app: &mut App, key: KeyEvent, ops: &mpsc::Sender<Op>) {
    if key.code == KeyCode::Esc {
        app.model_dialog = None;
        return;
    }

    if key.code == KeyCode::Enter {
        let model = app
            .model_dialog
            .as_ref()
            .and_then(ModelDialog::selected_model);
        if let Some(model) = model {
            app.model_dialog = None;
            app.model = model.clone();
            let models = app.models.clone();
            set_available_models(app, models);
            let _ = ops.send(Op::SetModel { model }).await;
        }
        return;
    }

    let Some(dialog) = app.model_dialog.as_mut() else {
        return;
    };
    match key.code {
        KeyCode::Up => dialog.selected = dialog.selected.saturating_sub(1),
        KeyCode::Down => {
            let maximum = dialog.matches().len().saturating_sub(1);
            dialog.selected = (dialog.selected + 1).min(maximum);
        }
        KeyCode::PageUp => dialog.selected = dialog.selected.saturating_sub(10),
        KeyCode::PageDown => {
            let maximum = dialog.matches().len().saturating_sub(1);
            dialog.selected = (dialog.selected + 10).min(maximum);
        }
        KeyCode::Home => dialog.selected = 0,
        KeyCode::End => {
            let maximum = dialog.matches().len().saturating_sub(1);
            dialog.selected = maximum;
        }
        KeyCode::Backspace => {
            dialog.query.pop();
            dialog.selected = 0;
        }
        KeyCode::Char(character)
            if !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            dialog.query.push(character);
            dialog.selected = 0;
        }
        _ => dialog.clamp_selection(),
    }
}

async fn handle_session_key(app: &mut App, key: KeyEvent, ops: &mpsc::Sender<Op>) {
    let Some(dialog) = app.session_dialog.as_mut() else {
        return;
    };

    // Rename mode: a small inline input for the selected session.
    if let Some(buffer) = dialog.renaming.as_mut() {
        match key.code {
            KeyCode::Esc => dialog.renaming = None,
            KeyCode::Backspace => {
                buffer.pop();
            }
            KeyCode::Enter => {
                if let Some(session) = dialog.sessions.get(dialog.selected) {
                    let _ = ops
                        .send(Op::RenameSession {
                            id: session.id.clone(),
                            title: buffer.clone(),
                        })
                        .await;
                }
                dialog.renaming = None;
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                buffer.push(character);
            }
            _ => {}
        }
        return;
    }

    match key.code {
        KeyCode::Esc | KeyCode::Char('q') => app.session_dialog = None,
        KeyCode::Up => dialog.selected = dialog.selected.saturating_sub(1),
        KeyCode::Down => {
            dialog.selected = (dialog.selected + 1).min(dialog.sessions.len().saturating_sub(1));
        }
        KeyCode::Home => dialog.selected = 0,
        KeyCode::End => dialog.selected = dialog.sessions.len().saturating_sub(1),
        KeyCode::Enter => {
            if let Some(session) = dialog.sessions.get(dialog.selected) {
                let id = session.id.clone();
                app.session_dialog = None;
                let _ = ops.send(Op::ResumeSession { id }).await;
            }
        }
        KeyCode::Char('r') => {
            let seed = dialog
                .sessions
                .get(dialog.selected)
                .and_then(|session| session.title.clone())
                .unwrap_or_default();
            dialog.renaming = Some(seed);
        }
        KeyCode::Char('d') => {
            if let Some(session) = dialog.sessions.get(dialog.selected) {
                let _ = ops
                    .send(Op::DeleteSession {
                        id: session.id.clone(),
                    })
                    .await;
            }
        }
        _ => {}
    }
}

/// Copy through OSC 52: the terminal owns the clipboard, so this works over
/// SSH and needs no display server or helper binary.
fn osc52_sequence(text: &str) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    format!("\x1b]52;c;{encoded}\x07")
}

fn last_assistant_text(app: &App) -> Option<String> {
    app.blocks.iter().rev().find_map(|block| match block {
        Block_::Assistant(text) if !text.trim().is_empty() => Some(text.clone()),
        _ => None,
    })
}

fn write_clipboard_helper(program: &str, arguments: &[&str], text: &str) -> std::io::Result<bool> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    if !Path::new(program).is_file() {
        return Ok(false);
    }
    let mut child = Command::new(program)
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes())?;
    }
    Ok(child.wait()?.success())
}

fn write_clipboard(text: &str) -> std::io::Result<&'static str> {
    use std::io::Write;

    for (program, arguments, label) in [
        ("/usr/bin/pbcopy", &[][..], "pbcopy"),
        (
            "/usr/bin/wl-copy",
            &["--type", "text/plain;charset=utf-8"][..],
            "wl-copy",
        ),
        (
            "/usr/bin/xclip",
            &["-selection", "clipboard", "-in"][..],
            "xclip",
        ),
        ("/usr/bin/xsel", &["--clipboard", "--input"][..], "xsel"),
    ] {
        if write_clipboard_helper(program, arguments, text)? {
            return Ok(label);
        }
    }

    let sequence = osc52_sequence(text);
    let mut stdout = std::io::stdout();
    stdout
        .write_all(sequence.as_bytes())
        .and_then(|_| stdout.flush())?;
    Ok("terminal OSC 52")
}

fn set_clipboard_notice(app: &mut App, result: std::io::Result<&'static str>, characters: usize) {
    let message = match result {
        Ok(backend) => format!("copied {characters} characters via {backend}"),
        Err(error) => format!("clipboard copy failed: {error}"),
    };
    app.clipboard_notice = Some((message, Instant::now()));
}

fn copy_last_assistant(app: &mut App) {
    let Some(text) = last_assistant_text(app) else {
        app.clipboard_notice = Some(("nothing to copy yet".into(), Instant::now()));
        return;
    };
    let characters = text.chars().count();
    let result = write_clipboard(&text);
    set_clipboard_notice(app, result, characters);
}

fn copy_selection_or_last_assistant(app: &mut App) {
    if app.selection.is_some_and(|selection| !selection.is_empty()) {
        copy_current_selection(app);
    } else {
        copy_last_assistant(app);
    }
}

fn copy_current_selection(app: &mut App) {
    let Some(text) = selected_text(app) else {
        return;
    };
    let characters = text.chars().count();
    let result = write_clipboard(&text);
    set_clipboard_notice(app, result, characters);
}

/// Re-render only the selected wrapped rows into small temporary buffers. This
/// gives clipboard extraction exactly the same wrapping as the visible
/// Paragraph without allocating a full-screen buffer for a huge transcript.
fn selected_text(app: &App) -> Option<String> {
    const CHUNK_ROWS: u16 = 256;
    const MAX_CLIPBOARD_BYTES: usize = 2 * 1024 * 1024;

    let selection = app.selection?;
    if selection.is_empty() || app.transcript_area.width == 0 {
        return None;
    }
    let (mut start, mut end) = selection.normalized();
    let width = app.transcript_area.width;
    let lines = transcript_lines(app);
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let rendered_rows = paragraph.line_count(width);
    if rendered_rows == 0 {
        return None;
    }
    let last_row = rendered_rows.saturating_sub(1).min(usize::from(u16::MAX)) as u16;
    start.row = start.row.min(last_row);
    end.row = end.row.min(last_row);
    start.column = start.column.min(width.saturating_sub(1));
    end.column = end.column.min(width.saturating_sub(1));

    let mut output = String::new();
    let mut row = start.row;
    loop {
        let rows_left = end.row.saturating_sub(row).saturating_add(1);
        let chunk_height = rows_left.min(CHUNK_ROWS);
        let area = Rect::new(0, 0, width, chunk_height);
        let mut buffer = ratatui::buffer::Buffer::empty(area);
        Paragraph::new(transcript_lines(app))
            .wrap(Wrap { trim: false })
            .scroll((row, 0))
            .render(area, &mut buffer);

        for offset in 0..chunk_height {
            let absolute_row = row.saturating_add(offset);
            let first_column = if absolute_row == start.row {
                start.column
            } else {
                0
            };
            let last_column = if absolute_row == end.row {
                end.column
            } else {
                width.saturating_sub(1)
            };
            for column in first_column..=last_column {
                if let Some(cell) = buffer.cell((column, offset)) {
                    output.push_str(cell.symbol());
                }
            }
            while output.ends_with(' ') {
                output.pop();
            }
            if absolute_row != end.row {
                output.push('\n');
            }
            if output.len() >= MAX_CLIPBOARD_BYTES {
                let mut boundary = MAX_CLIPBOARD_BYTES.min(output.len());
                while !output.is_char_boundary(boundary) {
                    boundary = boundary.saturating_sub(1);
                }
                output.truncate(boundary);
                return Some(output);
            }
        }

        if row.saturating_add(chunk_height) > end.row {
            break;
        }
        row = row.saturating_add(chunk_height);
    }
    Some(output)
}

fn insert(app: &mut App, c: char) {
    app.composer.insert(app.cursor, c);
    app.cursor += c.len_utf8();
    recompute_completions(app);
}

fn prev_boundary(s: &str, i: usize) -> usize {
    s[..i].chars().next_back().map_or(0, |c| i - c.len_utf8())
}

fn next_boundary(s: &str, i: usize) -> usize {
    s[i..].chars().next().map_or(i, |c| i + c.len_utf8())
}

/// Rows of the completion popup. The list scrolls inside this window, so the
/// number of commands is not limited by it.
const COMPLETION_ROWS: usize = 8;

fn recompute_completions(app: &mut App) {
    app.completions.clear();
    app.completion_idx = 0;
    app.completion_offset = 0;
    let t = app.composer.trim_start();
    if !t.starts_with('/') || t.contains(char::is_whitespace) {
        return;
    }
    // A bare `/` matches every command, which is what makes the menu a
    // browsable list rather than only a completer.
    app.completions = COMMANDS
        .iter()
        .filter(|(name, _)| name.starts_with(t))
        .map(|(name, _)| *name)
        .collect();

    // If a dedicated picker and a regular command share a prefix (`/mo`),
    // highlight the regular command so the inline suggestion still helps.
    // The bare `/` browser preserves the documented command order.
    if t != "/" {
        if let Some(index) = app
            .completions
            .iter()
            .position(|command| !DEDICATED_PICKER_COMMANDS.contains(command))
        {
            app.completion_idx = index;
        }
    }
}

/// Selected command offered as inline ghost text. A bare `/` is deliberately
/// kept as a browsable menu, while provider and model keep their richer
/// searchable pickers instead of using this generic completion path.
fn command_autosuggestion(app: &App) -> Option<&'static str> {
    if app.cursor != app.composer.len() {
        return None;
    }

    let typed = app.composer.trim_start();
    if typed == "/" || typed.contains(char::is_whitespace) {
        return None;
    }

    let command = app.completions.get(app.completion_idx).copied()?;
    if DEDICATED_PICKER_COMMANDS.contains(&command) || command == typed {
        return None;
    }

    command.starts_with(typed).then_some(command)
}

fn command_autosuggestion_suffix(app: &App) -> Option<&'static str> {
    let command = command_autosuggestion(app)?;
    command.strip_prefix(app.composer.trim_start())
}

/// Move the highlight by `delta`, wrapping at both ends, and keep the drawn
/// window around it.
fn move_completion(app: &mut App, delta: isize) {
    let len = app.completions.len();
    if len == 0 {
        return;
    }
    let len_i = len as isize;
    app.completion_idx = (((app.completion_idx as isize + delta) % len_i + len_i) % len_i) as usize;
    sync_completion_offset(app);
}

fn sync_completion_offset(app: &mut App) {
    let visible = COMPLETION_ROWS.min(app.completions.len().max(1));
    if app.completion_idx < app.completion_offset {
        app.completion_offset = app.completion_idx;
    } else if app.completion_idx >= app.completion_offset + visible {
        app.completion_offset = app.completion_idx + 1 - visible;
    }
}

fn accept_completion(app: &mut App) {
    if let Some(name) = app.completions.get(app.completion_idx) {
        app.composer = format!("{name} ");
        app.cursor = app.composer.len();
    }
    app.completions.clear();
    app.completion_offset = 0;
}

/// Every command and what it does, for `/help`.
fn help_text() -> String {
    let width = COMMANDS
        .iter()
        .map(|(name, _)| name.len())
        .max()
        .unwrap_or(10);
    let mut lines =
        vec!["commands — type / to browse them, ↑/↓ to move, Enter to pick".to_string()];
    for (name, description) in COMMANDS {
        lines.push(format!("  {name:<width$}  {description}"));
    }
    lines.push(String::new());
    lines.push("keys".into());
    for (keys, what) in [
        ("Enter", "send · Alt+Enter newline"),
        ("↑/↓", "command menu when open, otherwise input history"),
        (
            "Tab / Shift+Tab",
            "accept autosuggestion / previous command",
        ),
        (
            "Esc",
            "close the menu, clear a selection, jump to the latest message",
        ),
        ("Ctrl+C", "interrupt the running turn; exit when idle"),
        (
            "Ctrl+Y / Ctrl+Shift+C",
            "copy the selection or the last reply",
        ),
        ("PgUp/PgDn, Ctrl+↑/↓", "scroll the transcript"),
        ("Ctrl+Home / Ctrl+End", "jump to the start / end"),
    ] {
        lines.push(format!("  {keys:<21}  {what}"));
    }
    lines.join("\n")
}

async fn submit(app: &mut App, text: String, ops: &mpsc::Sender<Op>) {
    // TUI-local commands never reach the core.
    let trimmed = text.trim();
    match trimmed {
        "/quit" => {
            app.should_quit = true;
            return;
        }
        "/help" | "/?" | "/commands" => {
            app.blocks.push(Block_::Note(help_text()));
            return;
        }
        "/new" => {
            let _ = ops.send(Op::NewSession).await;
            return;
        }
        "/compact" => {
            let _ = ops.send(Op::Compact).await;
            return;
        }
        "/rollback" => {
            let _ = ops.send(Op::Rollback).await;
            return;
        }
        "/workspace" | "/cd" => {
            if app.recent_workspaces.is_empty() {
                app.blocks.push(Block_::Error(
                    "usage: /workspace PATH (alias: /cd PATH)".into(),
                ));
            } else {
                let mut lines = vec!["recent workspaces:".to_string()];
                for (index, path) in app.recent_workspaces.iter().enumerate() {
                    lines.push(format!("  {}. {path}", index + 1));
                }
                lines.push("switch with /workspace N or /workspace PATH".into());
                app.blocks.push(Block_::Note(lines.join("\n")));
            }
            return;
        }
        "/sessions" => {
            let _ = ops.send(Op::ListSessions).await;
            return;
        }
        "/fork" => {
            let _ = ops.send(Op::ForkSession).await;
            return;
        }
        "/resume" => {
            app.blocks.push(Block_::Error(
                "usage: /resume SESSION_ID (see /sessions)".into(),
            ));
            return;
        }
        "/memory" => {
            let _ = ops.send(Op::MemoryShow).await;
            return;
        }
        "/doctor" => {
            let _ = ops.send(Op::Doctor).await;
            return;
        }
        "/copy" => {
            copy_selection_or_last_assistant(app);
            return;
        }
        "/mouse" => {
            app.mouse_toggle = Some(!app.mouse_enabled);
            return;
        }
        "/contrast" => {
            app.high_contrast = !app.high_contrast;
            let state = if app.high_contrast { "on" } else { "off" };
            app.blocks.push(Block_::Note(format!(
                "high contrast {state} — borders and accents now use brighter colors"
            )));
            return;
        }
        "/tokens" => {
            show_token_usage(app);
            return;
        }
        "/notify" => {
            app.notifications_enabled = !app.notifications_enabled;
            let state = if app.notifications_enabled {
                "on"
            } else {
                "off"
            };
            app.blocks
                .push(Block_::Note(format!("desktop notifications {state}")));
            return;
        }
        "/diff" => {
            let _ = ops.send(Op::ShowDiff).await;
            return;
        }
        "/export" => {
            export_conversation(app);
            return;
        }
        "/clear" => {
            app.blocks.clear();
            app.scroll = 0;
            app.max_scroll = 0;
            app.detached = false;
            app.selection = None;
            app.blocks.push(Block_::Note(
                "transcript cleared — session history preserved".into(),
            ));
            return;
        }
        "/provider" => {
            let mut dialog = ProviderDialog::new();
            if let Some(index) = PROVIDERS
                .iter()
                .position(|provider| provider.name == app.provider)
            {
                dialog.selected = index;
            }
            app.provider_dialog = Some(dialog);
            return;
        }
        "/websearch" => {
            let enabled = !app.web_search_enabled;
            let _ = ops.send(Op::SetWebSearch { enabled }).await;
            return;
        }
        "/model" => {
            if app.models.is_empty() {
                app.blocks.push(Block_::Error(
                    "no model list is available; use /model MODEL".into(),
                ));
            } else {
                app.model_dialog = Some(ModelDialog::new(app.models.clone(), &app.model));
            }
            return;
        }
        "/sandbox" => {
            app.blocks.push(Block_::Error(
                "usage: /sandbox read-only|normal|full-access".into(),
            ));
            return;
        }
        "/skills" => {
            let _ = ops.send(Op::SkillsList).await;
            return;
        }
        "/skill" => {
            app.blocks.push(Block_::Error(
                "usage: /skill use|inspect|install|update|verify|remove ARG".into(),
            ));
            return;
        }
        _ => {}
    }

    if let Some(path) = workspace_command_path(trimmed) {
        // `/workspace 2` picks entry 2 from the recent list shown by bare
        // `/workspace`.
        let path = match path
            .to_str()
            .and_then(|raw| raw.parse::<usize>().ok())
            .filter(|n| (1..=app.recent_workspaces.len()).contains(n))
        {
            Some(index) => PathBuf::from(&app.recent_workspaces[index - 1]),
            None => path,
        };
        let _ = ops.send(Op::SetWorkspace { path }).await;
        return;
    }
    if let Some(id) = trimmed.strip_prefix("/resume ") {
        let id = id.trim().to_string();
        if id.is_empty() {
            app.blocks
                .push(Block_::Error("usage: /resume SESSION_ID".into()));
        } else {
            let _ = ops.send(Op::ResumeSession { id }).await;
        }
        return;
    }
    if let Some(argument) = trimmed.strip_prefix("/memory ") {
        let argument = argument.trim().to_ascii_lowercase();
        match argument.as_str() {
            "show" | "list" => {
                let _ = ops.send(Op::MemoryShow).await;
            }
            "status" => {
                let _ = ops.send(Op::MemoryStatus).await;
            }
            "dream" => {
                let _ = ops.send(Op::MemoryDream { dry_run: false }).await;
            }
            "dream --dry-run" | "dream --dry_run" | "dream dry-run" => {
                let _ = ops.send(Op::MemoryDream { dry_run: true }).await;
            }
            "reindex" => {
                let _ = ops.send(Op::MemoryReindex).await;
            }
            "clear" | "wipe" => {
                let _ = ops.send(Op::MemoryClear).await;
            }
            "on" | "enable" | "enabled" => {
                let _ = ops.send(Op::MemorySet { enabled: true }).await;
            }
            "off" | "disable" | "disabled" => {
                let _ = ops.send(Op::MemorySet { enabled: false }).await;
            }
            other => {
                if let Some(id) = other.strip_prefix("forget ") {
                    let id = id.trim();
                    if id.is_empty() {
                        app.blocks
                            .push(Block_::Error("usage: /memory forget FACT_ID".into()));
                    } else {
                        let _ = ops.send(Op::MemoryForget { id: id.to_string() }).await;
                    }
                } else {
                    app.blocks.push(Block_::Error(
                        "usage: /memory [status|show|dream [--dry-run]|reindex|forget ID|clear|on|off]"
                            .into(),
                    ));
                }
            }
        }
        return;
    }
    if let Some(argument) = trimmed.strip_prefix("/mouse ") {
        match argument.trim().to_ascii_lowercase().as_str() {
            "on" | "true" | "1" => app.mouse_toggle = Some(true),
            "off" | "false" | "0" => app.mouse_toggle = Some(false),
            _ => app
                .blocks
                .push(Block_::Error("usage: /mouse [on|off]".into())),
        }
        return;
    }
    if let Some(argument) = trimmed.strip_prefix("/notify ") {
        match argument.trim().to_ascii_lowercase().as_str() {
            "on" | "true" | "1" => app.notifications_enabled = true,
            "off" | "false" | "0" => app.notifications_enabled = false,
            _ => app
                .blocks
                .push(Block_::Error("usage: /notify [on|off]".into())),
        }
        return;
    }
    if let Some(model) = trimmed.strip_prefix("/model ") {
        let model = model.trim();
        if model.is_empty() {
            app.blocks.push(Block_::Error("usage: /model MODEL".into()));
        } else {
            app.model = model.to_string();
            let models = app.models.clone();
            set_available_models(app, models);
            let _ = ops
                .send(Op::SetModel {
                    model: model.to_string(),
                })
                .await;
        }
        return;
    }
    if let Some(mode) = trimmed.strip_prefix("/sandbox ") {
        let mode = mode.trim();
        if mode.is_empty() {
            app.blocks.push(Block_::Error(
                "usage: /sandbox read-only|normal|full-access".into(),
            ));
        } else {
            let _ = ops
                .send(Op::SetSandbox {
                    mode: mode.to_string(),
                })
                .await;
        }
        return;
    }
    if let Some(arguments) = trimmed.strip_prefix("/skill ") {
        let (action, value) = arguments
            .trim()
            .split_once(char::is_whitespace)
            .map(|(action, value)| (action, value.trim()))
            .unwrap_or((arguments.trim(), ""));
        if value.is_empty() {
            app.blocks.push(Block_::Error(
                "usage: /skill use|inspect|install|update|verify|remove ARG".into(),
            ));
            return;
        }
        let op = match action.to_ascii_lowercase().as_str() {
            "use" | "activate" => Op::SkillActivate {
                name: value.to_string(),
            },
            "show" | "inspect" => Op::SkillInspect {
                name: value.to_string(),
            },
            "install" => Op::SkillInstall {
                source: value.to_string(),
            },
            "update" => Op::SkillUpdate {
                name: value.to_string(),
            },
            "verify" => Op::SkillVerify {
                name: value.to_string(),
            },
            "remove" | "uninstall" => Op::SkillRemove {
                name: value.to_string(),
            },
            _ => {
                app.blocks.push(Block_::Error(
                    "usage: /skill use|inspect|install|update|verify|remove ARG".into(),
                ));
                return;
            }
        };
        let _ = ops.send(op).await;
        return;
    }
    if let Some(value) = trimmed.strip_prefix("/websearch ") {
        let enabled = match value.trim().to_ascii_lowercase().as_str() {
            "on" | "true" | "1" | "enable" | "enabled" => Some(true),
            "off" | "false" | "0" | "disable" | "disabled" => Some(false),
            _ => None,
        };
        if let Some(enabled) = enabled {
            let _ = ops.send(Op::SetWebSearch { enabled }).await;
        } else {
            app.blocks
                .push(Block_::Error("usage: /websearch on|off".into()));
        }
        return;
    }

    if let Some(path) = workspace_path_from_message(trimmed) {
        let _ = ops.send(Op::SetWorkspace { path }).await;
        return;
    }

    // Expand only markers registered by Ctrl+V. The transcript keeps a compact
    // attachment line, while the provider receives the actual image bytes as
    // an OpenAI-style multipart user turn.
    let mut display_text = text.clone();
    let mut model_text = text.clone();
    let mut image_parts = Vec::new();
    for path in image_marker_paths(&text) {
        let marker = format!("[image: {path}]");
        if !model_text.contains(&marker) {
            continue;
        }
        let Some(image) = app.pending_images.remove(&path) else {
            app.blocks.push(Block_::Error(
                "ignored an image marker that was not created by Ctrl+V".into(),
            ));
            continue;
        };

        display_text = display_text.replace(&marker, &format!("📎 {}", image.path.display()));
        model_text = model_text.replace(&marker, "");
        let bytes = tokio::fs::read(&image.path).await;
        let _ = tokio::fs::remove_file(&image.path).await;
        match bytes {
            Ok(bytes) if bytes.len() <= MAX_CLIPBOARD_IMAGE_BYTES => {
                let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
                image_parts.push(serde_json::json!({
                    "type": "image_url",
                    "image_url": {
                        "url": format!("data:{};base64,{encoded}", image.media_type),
                    }
                }));
                app.blocks.push(Block_::Note(format!(
                    "image attached: {}",
                    image.path.display()
                )));
            }
            Ok(bytes) => app.blocks.push(Block_::Error(format!(
                "clipboard image is too large ({} MiB; limit {} MiB)",
                bytes.len() / (1024 * 1024),
                MAX_CLIPBOARD_IMAGE_BYTES / (1024 * 1024),
            ))),
            Err(error) => app.blocks.push(Block_::Error(format!(
                "cannot read pasted image {}: {error}",
                image.path.display()
            ))),
        }
    }

    if !image_parts.is_empty() {
        let prompt = if model_text.trim().is_empty() {
            "Analyze the attached image.".to_string()
        } else {
            model_text.trim().to_string()
        };
        let mut parts = vec![serde_json::json!({
            "type": "text",
            "text": prompt,
        })];
        parts.extend(image_parts);
        model_text = serde_json::to_string(&parts).expect("multipart user content is serializable");
    }

    app.blocks.push(Block_::User(display_text));
    let _ = ops.send(Op::Submit { text: model_text }).await;
}

fn workspace_command_path(text: &str) -> Option<PathBuf> {
    ["/workspace", "/cd"].into_iter().find_map(|command| {
        let rest = text.strip_prefix(command)?;
        if !rest.starts_with(char::is_whitespace) {
            return None;
        }
        let path = unquote_path(rest.trim());
        (!path.is_empty()).then(|| PathBuf::from(path))
    })
}

/// Recognise an explicit request to move the coding workspace before it
/// reaches the model. A shell `cd` cannot persist across tool calls, so this
/// intent must be handled by the application itself.
fn workspace_path_from_message(text: &str) -> Option<PathBuf> {
    let lower = text.to_lowercase();
    let names_workspace = [
        "workspace",
        "folder",
        "director",
        "proiect",
        "directory",
        "project",
    ]
    .iter()
    .any(|word| lower.contains(word));
    if !names_workspace {
        return None;
    }

    let requests_change = [
        "schimb", "mută", "muta", "seteaz", "change", "switch", "move", "set the",
    ]
    .iter()
    .any(|phrase| lower.contains(phrase));
    let states_location = [
        "proiectul meu este",
        "proiectul meu e",
        "folderul meu este",
        "folderul meu e",
        "directorul meu este",
        "directorul meu e",
        "my project is",
        "my folder is",
        "my directory is",
        "workspace is",
        "workspace-ul este",
        "workspace-ul e",
    ]
    .iter()
    .any(|phrase| lower.contains(phrase));
    let concise_assignment = lower.trim_start().starts_with("workspace:")
        || lower.trim_start().starts_with("folder:")
        || lower.trim_start().starts_with("director:")
        || lower.trim_start().starts_with("project:");
    if !(requests_change || states_location || concise_assignment) {
        return None;
    }

    explicit_path(text).map(PathBuf::from)
}

fn explicit_path(text: &str) -> Option<String> {
    for quote in ['"', '\''] {
        let Some(start) = text.find(quote) else {
            continue;
        };
        let after = &text[start + quote.len_utf8()..];
        let Some(end) = after.find(quote) else {
            continue;
        };
        let candidate = after[..end].trim();
        if looks_like_path(candidate) {
            return Some(candidate.to_string());
        }
    }

    text.split_whitespace().find_map(|part| {
        let candidate = part.trim_matches(|character: char| {
            matches!(
                character,
                '"' | '\'' | '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';' | ':' | '!' | '?'
            )
        });
        let candidate = candidate.strip_suffix('.').unwrap_or(candidate);
        looks_like_path(candidate).then(|| candidate.to_string())
    })
}

fn looks_like_path(value: &str) -> bool {
    value.starts_with('/')
        || value == "~"
        || value.starts_with("~/")
        || value == "."
        || value == ".."
        || value.starts_with("./")
        || value.starts_with("../")
}

fn unquote_path(value: &str) -> &str {
    if value.len() >= 2 {
        let first = value.as_bytes()[0];
        let last = value.as_bytes()[value.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return &value[1..value.len() - 1];
        }
    }
    value
}

// ---------------------------------------------------------------------------
// Draw
// ---------------------------------------------------------------------------

/// Visual rows the composer may occupy before it starts scrolling instead of
/// growing. Past this it would eat the transcript.
const COMPOSER_MAX_ROWS: u16 = 8;

/// The composer laid out as it is drawn.
///
/// Height, rendering and the caret all read from here. Deriving them
/// separately is what put the caret past the right edge: the box was sized by
/// counting `\n`, while the text was word-wrapped, so every row a long line
/// wrapped onto fell outside the border.
struct WrappedComposer {
    /// Byte ranges into the composer text, one per visual row. Every byte
    /// belongs to exactly one row, which is what keeps the caret exact.
    rows: Vec<std::ops::Range<usize>>,
    cursor_row: u16,
    cursor_col: u16,
}

/// Break one hard line into visual rows of at most `width` characters.
///
/// Breaks at the last space that fits; a word longer than the row is split
/// mid-word, because refusing to split it would overflow the border again.
fn wrap_line(line: &str, width: usize) -> Vec<std::ops::Range<usize>> {
    if line.is_empty() {
        return vec![0..0];
    }
    let mut rows = Vec::new();
    let mut row_start = 0usize;
    let mut last_break: Option<usize> = None;
    let mut columns = 0usize;

    for (offset, character) in line.char_indices() {
        if columns == width {
            // Keep the space on the row it ended, so no byte is lost between
            // rows and the caret mapping stays one-to-one.
            let cut = last_break
                .filter(|value| *value > row_start)
                .unwrap_or(offset);
            rows.push(row_start..cut);
            row_start = cut;
            last_break = None;
            columns = line[row_start..offset].chars().count();
        }
        if character == ' ' {
            last_break = Some(offset + character.len_utf8());
        }
        columns += 1;
    }
    rows.push(row_start..line.len());
    rows
}

fn wrap_composer(text: &str, cursor: usize, width: u16) -> WrappedComposer {
    let width = width.max(1) as usize;
    let mut rows: Vec<std::ops::Range<usize>> = Vec::new();
    let mut cursor_row = 0u16;
    let mut cursor_col = 0u16;
    let mut located = false;
    let mut line_start = 0usize;

    for hard_line in text.split('\n') {
        for row in wrap_line(hard_line, width) {
            let start = line_start + row.start;
            let end = line_start + row.end;
            if !located && cursor >= start && cursor <= end {
                cursor_row = rows.len() as u16;
                cursor_col = text[start..cursor].chars().count() as u16;
                located = true;
            }
            rows.push(start..end);
        }
        // Past the '\n' that `split` consumed.
        line_start += hard_line.len() + 1;
    }

    // A caret sitting exactly where a full row ends belongs at the start of the
    // next one, otherwise it is drawn on the border.
    if cursor_col as usize >= width {
        cursor_row += 1;
        cursor_col = 0;
        if rows.len() <= cursor_row as usize {
            let end = text.len();
            rows.push(end..end);
        }
    }

    WrappedComposer {
        rows,
        cursor_row,
        cursor_col,
    }
}

/// The composer text as drawn, plus the byte length of the part the user
/// actually typed — everything after it is the dimmed autosuggestion.
fn composer_text(app: &App) -> (String, usize) {
    let typed = app.composer.len();
    match command_autosuggestion_suffix(app) {
        Some(suffix) => (format!("{}{suffix}", app.composer), typed),
        None => (app.composer.clone(), typed),
    }
}

fn draw(f: &mut Frame, app: &mut App) {
    // Composer grows with content, capped so it never eats the transcript.
    let (text, _) = composer_text(app);
    let rows = wrap_composer(&text, app.cursor, f.area().width.saturating_sub(2))
        .rows
        .len() as u16;
    let composer_h = rows.clamp(1, COMPOSER_MAX_ROWS) + 2;

    let [transcript, status, composer] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(composer_h),
    ])
    .areas(f.area());

    draw_transcript(f, app, transcript);
    draw_status(f, app, status);
    draw_composer(f, app, composer);

    if !app.completions.is_empty() {
        draw_completions(f, app, composer);
    }
    if let Some((_, body, allow_always)) = &app.pending_approval {
        draw_approval(f, body, *allow_always, f.area());
    }
    if let Some(dialog) = &app.provider_dialog {
        draw_provider_dialog(f, dialog, f.area());
    }
    if let Some(dialog) = &app.model_dialog {
        draw_model_dialog(f, dialog, f.area());
    }
    if let Some(dialog) = &app.session_dialog {
        draw_session_dialog(f, dialog, f.area());
    }
    if let Some(dialog) = &app.privilege_dialog {
        draw_privilege_dialog(f, dialog, f.area());
    }
}

fn draw_model_dialog(f: &mut Frame, dialog: &ModelDialog, screen: Rect) {
    let matches = dialog.matches();
    let width = screen.width.min(96);
    let height = screen.height.min((matches.len() as u16 + 7).clamp(9, 24));
    let area = Rect {
        x: screen.x + screen.width.saturating_sub(width) / 2,
        y: screen.y + screen.height.saturating_sub(height) / 2,
        width,
        height,
    };

    f.render_widget(Clear, area);
    let outer = Block::default()
        .borders(Borders::ALL)
        .title(" model ")
        .border_style(Style::new().fg(Color::Cyan));
    let inner = outer.inner(area);
    f.render_widget(outer, area);

    let [query_area, list_area, footer] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(1),
        Constraint::Length(2),
    ])
    .areas(inner);
    f.render_widget(
        Paragraph::new(format!("filter: {}▏", dialog.query)).style(Style::new().fg(Color::Cyan)),
        query_area,
    );

    if matches.is_empty() {
        f.render_widget(Paragraph::new("no matching models"), list_area);
    } else {
        let visible = list_area.height.max(1) as usize;
        let start = dialog
            .selected
            .saturating_sub(visible / 2)
            .min(matches.len().saturating_sub(visible));
        let items = matches
            .iter()
            .enumerate()
            .skip(start)
            .take(visible)
            .map(|(index, model)| {
                let style = if index == dialog.selected {
                    Style::new().bg(Color::DarkGray).bold()
                } else {
                    Style::new()
                };
                ListItem::new(Line::styled((*model).to_string(), style))
            })
            .collect::<Vec<_>>();
        f.render_widget(List::new(items), list_area);
    }

    f.render_widget(
        Paragraph::new(format!(
            "{} model(s)   type to filter   ↑/↓ select   Enter use   Esc close",
            matches.len()
        ))
        .style(Style::new().fg(Color::DarkGray)),
        footer,
    );
}

fn draw_session_dialog(f: &mut Frame, dialog: &SessionDialog, screen: Rect) {
    let width = screen.width.min(100);
    let height = screen
        .height
        .min((dialog.sessions.len() as u16 + 5).clamp(7, 22));
    let area = Rect {
        x: screen.x + screen.width.saturating_sub(width) / 2,
        y: screen.y + screen.height.saturating_sub(height) / 2,
        width,
        height,
    };

    f.render_widget(Clear, area);
    let outer = Block::default()
        .borders(Borders::ALL)
        .title(" sessions ")
        .border_style(Style::new().fg(Color::Cyan));
    let inner = outer.inner(area);
    f.render_widget(outer, area);

    let [list_area, footer] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).areas(inner);

    if dialog.sessions.is_empty() {
        f.render_widget(Paragraph::new("no saved sessions"), list_area);
    } else {
        let visible = list_area.height.max(1) as usize;
        let start = dialog
            .selected
            .saturating_sub(visible / 2)
            .min(dialog.sessions.len().saturating_sub(visible));
        let items = dialog
            .sessions
            .iter()
            .enumerate()
            .skip(start)
            .take(visible)
            .map(|(index, session)| {
                let marker = if session.is_current { "●" } else { " " };
                let title = session
                    .title
                    .clone()
                    .unwrap_or_else(|| session.id.chars().take(8).collect());
                let workspace = session
                    .workspace
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_else(|| session.workspace.display().to_string());
                let style = if index == dialog.selected {
                    Style::new().bg(Color::DarkGray).bold()
                } else {
                    Style::new()
                };
                ListItem::new(Line::styled(
                    format!(
                        "{marker} {:<28} {:<18} {:<20} {:>4} turns",
                        title.chars().take(28).collect::<String>(),
                        workspace.chars().take(18).collect::<String>(),
                        session.model.chars().take(20).collect::<String>(),
                        session.turns
                    ),
                    style,
                ))
            })
            .collect::<Vec<_>>();
        f.render_widget(List::new(items), list_area);
    }

    let footer_text = match &dialog.renaming {
        Some(buffer) => format!("new title: {buffer}▏   Enter save   Esc cancel"),
        None => "Enter resume   r rename   d delete   Esc close".to_string(),
    };
    f.render_widget(
        Paragraph::new(footer_text).style(Style::new().fg(Color::DarkGray)),
        footer,
    );
}

fn transcript_lines(app: &App) -> Vec<Line<'static>> {
    let mut lines = crate::banner::banner(
        env!("CARGO_PKG_VERSION"),
        &app.model,
        &app.workspace,
        &app.sandbox,
    );
    lines.push(Line::raw(""));

    for block in &app.blocks {
        match block {
            Block_::User(t) => {
                lines.push(Line::from(vec![
                    Span::styled("› ", Style::new().fg(Color::Cyan).bold()),
                    Span::styled(t.clone(), Style::new().bold()),
                ]));
            }
            Block_::Assistant(t) => {
                for l in t.lines() {
                    lines.push(Line::raw(l.to_string()));
                }
            }
            Block_::Reasoning(t) => {
                for l in t.lines().take(3) {
                    lines.push(Line::styled(
                        format!("  {l}"),
                        Style::new().fg(Color::DarkGray).italic(),
                    ));
                }
            }
            Block_::Tool {
                name,
                summary,
                output,
                done,
                ok,
                ms,
            } => {
                let (mark, color) = match (done, ok) {
                    (false, _) => ("·", Color::Yellow),
                    (true, true) => ("✓", Color::Green),
                    (true, false) => ("✗", Color::Red),
                };
                let timing = if *done {
                    format!(" {ms}ms")
                } else {
                    String::new()
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("{mark} "), Style::new().fg(color)),
                    Span::styled(name.clone(), Style::new().fg(Color::Blue)),
                    Span::raw(format!(" {summary}{timing}")),
                ]));
                // Only tail the output of a running command; a finished one is
                // noise unless it failed.
                if !*done || !*ok {
                    for l in output
                        .lines()
                        .rev()
                        .take(6)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                    {
                        lines.push(Line::styled(
                            format!("    {l}"),
                            Style::new().fg(Color::DarkGray),
                        ));
                    }
                }
            }
            Block_::Diff(d) => {
                for l in d.lines() {
                    let style = match l.chars().next() {
                        Some('+') => Style::new().fg(Color::Green),
                        Some('-') => Style::new().fg(Color::Red),
                        Some('@') => Style::new().fg(Color::Cyan),
                        _ => Style::new().fg(Color::DarkGray),
                    };
                    lines.push(Line::styled(l.to_string(), style));
                }
            }
            Block_::Verify {
                stage,
                passed,
                summary,
            } => {
                let color = if *passed { Color::Green } else { Color::Red };
                lines.push(Line::from(vec![
                    Span::styled(format!("[{stage}] "), Style::new().fg(color).bold()),
                    Span::raw(summary.clone()),
                ]));
            }
            // Notes and errors can be multi-line (doctor reports, memory
            // listings); a Line must not contain embedded newlines.
            Block_::Error(e) => {
                for (index, l) in e.lines().enumerate() {
                    let prefix = if index == 0 { "error: " } else { "       " };
                    lines.push(Line::styled(
                        format!("{prefix}{l}"),
                        Style::new().fg(Color::Red),
                    ));
                }
            }
            Block_::Note(n) => {
                for (index, l) in n.lines().enumerate() {
                    let prefix = if index == 0 { "— " } else { "  " };
                    lines.push(Line::styled(
                        format!("{prefix}{l}"),
                        Style::new().fg(Color::DarkGray),
                    ));
                }
            }
        }
        lines.push(Line::raw(""));
    }
    lines
}

fn draw_transcript(f: &mut Frame, app: &mut App, area: Rect) {
    app.transcript_area = area;
    let lines = transcript_lines(app);
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    app.max_scroll = transcript_max_scroll(&paragraph, area);
    app.scroll = transcript_scroll(&paragraph, area, app.scroll, app.detached);
    if app.detached && app.scroll >= app.max_scroll {
        app.detached = false;
    }

    f.render_widget(paragraph.scroll((app.scroll, 0)), area);

    if let Some(selection) = app.selection {
        f.render_widget(
            SelectionHighlight {
                selection,
                scroll: app.scroll,
                reserve_scrollbar: app.max_scroll > 0,
            },
            area,
        );
    }

    // Position indicator: a scrollbar whenever the transcript overflows.
    if app.max_scroll > 0 {
        let mut scrollbar_state =
            ScrollbarState::new(app.max_scroll as usize).position(app.scroll as usize);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            area,
            &mut scrollbar_state,
        );
    }
}

struct SelectionHighlight {
    selection: TextSelection,
    scroll: u16,
    reserve_scrollbar: bool,
}

impl Widget for SelectionHighlight {
    fn render(self, area: Rect, buffer: &mut ratatui::buffer::Buffer) {
        let (start, end) = self.selection.normalized();
        let content_width = area.width.saturating_sub(u16::from(self.reserve_scrollbar));
        if content_width == 0 {
            return;
        }

        for screen_row in 0..area.height {
            let absolute_row = self.scroll.saturating_add(screen_row);
            if absolute_row < start.row || absolute_row > end.row {
                continue;
            }
            let first_column = if absolute_row == start.row {
                start.column.min(content_width.saturating_sub(1))
            } else {
                0
            };
            let last_column = if absolute_row == end.row {
                end.column.min(content_width.saturating_sub(1))
            } else {
                content_width.saturating_sub(1)
            };
            for column in first_column..=last_column {
                if let Some(cell) = buffer.cell_mut((
                    area.x.saturating_add(column),
                    area.y.saturating_add(screen_row),
                )) {
                    cell.set_bg(Color::Blue);
                    cell.set_fg(Color::White);
                }
            }
        }
    }
}

/// 0–100, where 100 means pinned to the newest output.
fn scroll_percent(scroll: u16, max_scroll: u16) -> u16 {
    if max_scroll == 0 {
        100
    } else {
        (u32::from(scroll) * 100 / u32::from(max_scroll)) as u16
    }
}

/// `Text::height()` and `Vec<Line>::len()` only count explicit newlines. Model
/// output often streams as one long logical line, which Ratatui wraps into many
/// terminal rows. Using `Paragraph::line_count` keeps the viewport attached to
/// the actual rendered tail as every token arrives.
fn transcript_max_scroll(paragraph: &Paragraph<'_>, area: Rect) -> u16 {
    let rendered_rows = paragraph.line_count(area.width);
    let max_scroll = rendered_rows.saturating_sub(usize::from(area.height));
    max_scroll.min(usize::from(u16::MAX)) as u16
}

fn transcript_scroll(
    paragraph: &Paragraph<'_>,
    area: Rect,
    current_top: u16,
    detached: bool,
) -> u16 {
    let max_scroll = transcript_max_scroll(paragraph, area);
    if detached {
        current_top.min(max_scroll)
    } else {
        max_scroll
    }
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let spinner = if app.busy {
        const FRAMES: [&str; 4] = ["⠋", "⠙", "⠹", "⠸"];
        let i = app
            .turn_started
            .map(|started| (started.elapsed().as_millis() / 100) as usize)
            .unwrap_or(0);
        FRAMES[i % FRAMES.len()]
    } else {
        "•"
    };

    let elapsed = app
        .turn_started
        .map(|t| format!(" {:.0}s", t.elapsed().as_secs_f32()))
        .unwrap_or_default();

    let queued = if app.queue.is_empty() {
        String::new()
    } else {
        format!("  +{} queued", app.queue.len())
    };

    let branch = app
        .branch
        .as_ref()
        .map(|b| format!("  {b}"))
        .unwrap_or_default();

    let web = if app.web_search_enabled {
        "web:on"
    } else {
        "web:off"
    };
    // Scroll position: visible only while detached from the tail, which is
    // exactly when you have lost track of where you are.
    let scroll_pos = if app.detached {
        format!(
            "  · scroll {}% (Esc ↓)",
            scroll_percent(app.scroll, app.max_scroll)
        )
    } else {
        String::new()
    };
    let mouse = if app.mouse_enabled { "" } else { "  mouse:off" };

    // Search mode indicator
    let search = if app.search_mode {
        if app.search_matches.is_empty() {
            format!("  [search: {} (no matches)]", app.search_query)
        } else {
            format!(
                "  [search: {} ({}/{})]",
                app.search_query,
                app.search_idx + 1,
                app.search_matches.len()
            )
        }
    } else {
        String::new()
    };

    let left = app
        .clipboard_notice
        .as_ref()
        .filter(|(_, created)| created.elapsed() < Duration::from_secs(3))
        .map(|(message, _)| format!("✓ {message}"))
        .unwrap_or_else(|| {
            format!(
                "{spinner}{elapsed}  {} · {}  {} · {web}{branch}{queued}{mouse}{search}{scroll_pos}",
                app.provider, app.model, app.sandbox
            )
        });
    let right = format!("↑{} ↓{}", app.tokens_in, app.tokens_out);

    let pad = (area.width as usize)
        .saturating_sub(left.len() + right.len())
        .max(1);

    let status_fg = if app.high_contrast {
        Color::Gray
    } else {
        Color::DarkGray
    };
    f.render_widget(
        Paragraph::new(Line::styled(
            format!("{left}{}{right}", " ".repeat(pad)),
            Style::new().fg(status_fg),
        )),
        area,
    );
}

fn draw_composer(f: &mut Frame, app: &App, area: Rect) {
    let border = if app.busy {
        Color::DarkGray
    } else if app.high_contrast {
        Color::LightCyan
    } else {
        Color::Cyan
    };

    let (text, typed) = composer_text(app);
    let inner_width = area.width.saturating_sub(2);
    let inner_height = area.height.saturating_sub(2);
    let wrapped = wrap_composer(&text, app.cursor, inner_width);
    let suggestion = Style::new().fg(if app.high_contrast {
        Color::Gray
    } else {
        Color::DarkGray
    });

    // The rows are already wrapped, so the paragraph must not wrap them again.
    let lines: Vec<Line> = wrapped
        .rows
        .iter()
        .map(|row| {
            let (start, end) = (row.start, row.end);
            if end <= typed {
                Line::from(Span::raw(text[start..end].to_string()))
            } else if start >= typed {
                Line::from(Span::styled(text[start..end].to_string(), suggestion))
            } else {
                // The autosuggestion begins partway through this row.
                Line::from(vec![
                    Span::raw(text[start..typed].to_string()),
                    Span::styled(text[typed..end].to_string(), suggestion),
                ])
            }
        })
        .collect();

    // Past the cap the composer scrolls instead of growing, so keep the caret
    // on screen rather than letting it run off the bottom.
    let scroll = wrapped
        .cursor_row
        .saturating_sub(inner_height.saturating_sub(1));

    f.render_widget(
        Paragraph::new(Text::from(lines)).scroll((scroll, 0)).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::new().fg(border)),
        ),
        area,
    );

    f.set_cursor_position((
        area.x + 1 + wrapped.cursor_col,
        area.y + 1 + wrapped.cursor_row - scroll,
    ));
}

fn draw_completions(f: &mut Frame, app: &App, composer: Rect) {
    let total = app.completions.len();
    if total == 0 {
        return;
    }
    let visible = COMPLETION_ROWS.min(total);
    let height = visible as u16 + 2;
    // Descriptions carry the value here, so give the popup room for them
    // rather than the old fixed 50 columns.
    let width = composer.width.min(84).max(30);
    let area = Rect {
        x: composer.x,
        y: composer.y.saturating_sub(height),
        width,
        height,
    };

    let offset = app.completion_offset.min(total.saturating_sub(visible));
    let name_width = app
        .completions
        .iter()
        .map(|name| name.len())
        .max()
        .unwrap_or(10);
    let items: Vec<ListItem> = app
        .completions
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible)
        .map(|(index, name)| {
            let description = COMMANDS
                .iter()
                .find(|(candidate, _)| candidate == name)
                .map(|(_, description)| *description)
                .unwrap_or("");
            let selected = index == app.completion_idx;
            let marker = if selected { "❯ " } else { "  " };
            let style = if selected {
                Style::new().bg(Color::DarkGray).bold()
            } else {
                Style::new()
            };
            ListItem::new(Line::styled(
                format!("{marker}{name:<name_width$}  {description}"),
                style,
            ))
        })
        .collect();

    // The title doubles as the scroll indicator when the list overflows.
    let title = if total > visible {
        format!(
            " commands {}/{} · ↑↓ move · Enter pick · Esc close ",
            app.completion_idx + 1,
            total
        )
    } else {
        " commands · ↑↓ move · Enter pick · Esc close ".to_string()
    };

    f.render_widget(Clear, area);
    f.render_widget(
        List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .title(Line::styled(title, Style::new().fg(Color::DarkGray))),
        ),
        area,
    );
}

fn draw_approval(f: &mut Frame, body: &str, allow_always: bool, screen: Rect) {
    let w = screen.width.min(70);
    let h = screen.height.min(12);
    let area = Rect {
        x: (screen.width - w) / 2,
        y: (screen.height - h) / 2,
        width: w,
        height: h,
    };

    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(if allow_always {
            format!("{body}\n\n[y] allow   [a] always   [n] deny")
        } else {
            format!("{body}\n\n[y] allow once   [n] deny")
        })
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" approval required ")
                .border_style(Style::new().fg(Color::Yellow)),
        ),
        area,
    );
}

fn draw_privilege_dialog(f: &mut Frame, dialog: &PrivilegeDialog, screen: Rect) {
    let width = screen.width.min(78);
    let height = screen.height.min(16);
    let area = Rect {
        x: screen.x + screen.width.saturating_sub(width) / 2,
        y: screen.y + screen.height.saturating_sub(height) / 2,
        width,
        height,
    };
    let remembered = if !dialog.keyring_available {
        "Keyring indisponibil: parola rămâne numai în memorie pentru această autentificare."
            .to_string()
    } else if dialog.remember {
        "[x] Salvează în keyring-ul desktop (Tab schimbă)".to_string()
    } else {
        "[ ] Salvează în keyring-ul desktop (Tab schimbă)".to_string()
    };
    let message = dialog
        .message
        .as_deref()
        .map(|text| format!("\n{text}\n"))
        .unwrap_or_default();
    let masked = "•".repeat(dialog.input.chars().count());
    let body = format!(
        "Comandă root:\n{}\n{}\nParola sudo (încercarea {}/3):\n{}\n\n{}\n\nEnter confirmă · Esc anulează\nParola nu este trimisă modelului.",
        dialog.command, message, dialog.attempt, masked, remembered
    );

    f.render_widget(Clear, area);
    f.render_widget(
        Paragraph::new(body).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" autentificare sudo ")
                .border_style(Style::new().fg(Color::Yellow)),
        ),
        area,
    );
}

fn draw_provider_dialog(f: &mut Frame, dialog: &ProviderDialog, screen: Rect) {
    let width = screen.width.min(88);
    let desired_height = match dialog.stage {
        ProviderStage::Select => (PROVIDERS.len() as u16 + 4).min(25),
        ProviderStage::BaseUrl | ProviderStage::ApiKey => 10,
    };
    let height = screen.height.min(desired_height.max(6));
    let area = Rect {
        x: screen.x + screen.width.saturating_sub(width) / 2,
        y: screen.y + screen.height.saturating_sub(height) / 2,
        width,
        height,
    };

    f.render_widget(Clear, area);
    let outer = Block::default()
        .borders(Borders::ALL)
        .title(" provider ")
        .border_style(Style::new().fg(Color::Cyan));
    let inner = outer.inner(area);
    f.render_widget(outer, area);

    match dialog.stage {
        ProviderStage::Select => {
            let footer_height = 2;
            let [list_area, footer] =
                Layout::vertical([Constraint::Min(1), Constraint::Length(footer_height)])
                    .areas(inner);
            let visible = list_area.height.max(1) as usize;
            let start = dialog
                .selected
                .saturating_sub(visible / 2)
                .min(PROVIDERS.len().saturating_sub(visible));
            let items = PROVIDERS
                .iter()
                .enumerate()
                .skip(start)
                .take(visible)
                .map(|(index, provider)| {
                    let auth = match provider.auth {
                        AuthKind::ApiKey => "API key",
                        AuthKind::Account => "account",
                        AuthKind::OptionalApiKey => "optional key",
                    };
                    let style = if index == dialog.selected {
                        Style::new().bg(Color::DarkGray).bold()
                    } else {
                        Style::new()
                    };
                    ListItem::new(Line::styled(
                        format!(
                            "{:<30} {:<12} {}",
                            provider.name, auth, provider.description
                        ),
                        style,
                    ))
                })
                .collect::<Vec<_>>();
            f.render_widget(List::new(items), list_area);
            f.render_widget(
                Paragraph::new("↑/↓ select   Enter continue   Esc cancel")
                    .style(Style::new().fg(Color::DarkGray)),
                footer,
            );
        }
        ProviderStage::BaseUrl | ProviderStage::ApiKey => {
            let selected = &PROVIDERS[dialog.selected];
            let is_key = matches!(dialog.stage, ProviderStage::ApiKey);
            let label = if is_key {
                if selected.auth == AuthKind::OptionalApiKey {
                    "API key (optional; blank reuses the saved key)"
                } else {
                    "API key (blank reuses the saved key)"
                }
            } else {
                "OpenAI-compatible base URL"
            };
            let value = if is_key {
                "•".repeat(dialog.input.chars().count())
            } else {
                dialog.input.clone()
            };
            let error = dialog
                .error
                .as_deref()
                .map(|message| format!("\n\n{message}"))
                .unwrap_or_default();
            f.render_widget(
                Paragraph::new(format!(
                    "{}\n\n{label}:\n{value}{error}\n\nEnter confirm   Esc cancel",
                    selected.name
                ))
                .wrap(Wrap { trim: false })
                .style(if dialog.error.is_some() {
                    Style::new().fg(Color::Red)
                } else {
                    Style::new()
                }),
                inner,
            );
        }
    }
}

/// Update search matches based on current query
fn update_search_matches(app: &mut App) {
    app.search_matches.clear();
    app.search_idx = 0;

    if app.search_query.is_empty() {
        return;
    }

    let query = app.search_query.to_lowercase();

    for (idx, block) in app.blocks.iter().enumerate() {
        let text = match block {
            Block_::User(t) | Block_::Assistant(t) | Block_::Reasoning(t) => t,
            Block_::Tool {
                name,
                summary,
                output,
                ..
            } => &format!("{} {} {}", name, summary, output),
            Block_::Diff(t) => t,
            Block_::Verify { stage, summary, .. } => &format!("{} {}", stage, summary),
            Block_::Error(t) | Block_::Note(t) => t,
        };

        if text.to_lowercase().contains(&query) {
            app.search_matches.push(idx);
        }
    }
}

/// Scroll the transcript so the current search match is visible.
fn scroll_to_match(app: &mut App) {
    let Some(&block_idx) = app.search_matches.get(app.search_idx) else {
        return;
    };

    // Rebuild the transcript lines to find the row offset for this block
    let lines = transcript_lines(app);
    let mut target_row = 0usize;
    let mut current_block = 0usize;

    for line in &lines {
        if current_block == block_idx {
            break;
        }
        // Count rows consumed by this line (accounting for wrapping)
        let line_width = line.width();
        let area_width = app.transcript_area.width.max(1) as usize;
        let rows = if line_width == 0 {
            1
        } else {
            (line_width + area_width - 1) / area_width
        };
        target_row += rows;

        // Detect block boundaries by empty lines (blocks are separated by blank lines)
        if line_width == 0 {
            current_block += 1;
        }
    }

    // Set scroll to show the match (with some context above)
    let context_rows = 3usize;
    let scroll_target = target_row.saturating_sub(context_rows);
    app.scroll = scroll_target.min(usize::from(u16::MAX)) as u16;
    app.detached = true; // Prevent auto-scroll from overriding our position
}

const MAX_CLIPBOARD_IMAGE_BYTES: usize = 20 * 1024 * 1024;
const CLIPBOARD_COMMAND_TIMEOUT: Duration = Duration::from_secs(3);

fn image_marker_paths(text: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("[image: ") {
        let after = &rest[start + 8..];
        let Some(end) = after.find(']') else {
            break;
        };
        let path = after[..end].trim();
        if !path.is_empty() && !paths.iter().any(|known| known == path) {
            paths.push(path.to_string());
        }
        rest = &after[end + 1..];
    }
    paths
}

fn advertised_image_type(types: &[u8]) -> Option<(&'static str, &'static str)> {
    let types = String::from_utf8_lossy(types);
    [
        ("image/png", "png"),
        ("image/jpeg", "jpg"),
        ("image/webp", "webp"),
        ("image/gif", "gif"),
    ]
    .into_iter()
    .find(|(media_type, _)| types.lines().any(|line| line.trim() == *media_type))
}

async fn clipboard_command(program: &str, args: &[&str]) -> Option<Vec<u8>> {
    let output = tokio::time::timeout(
        CLIPBOARD_COMMAND_TIMEOUT,
        tokio::process::Command::new(program).args(args).output(),
    )
    .await
    .ok()?
    .ok()?;
    output.status.success().then_some(output.stdout)
}

#[cfg(target_os = "macos")]
async fn clipboard_image_macos() -> std::result::Result<Option<PendingImage>, String> {
    let dir = std::env::temp_dir().join("gnomef-paste");
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    let path = dir.join(format!("paste-{}.png", uuid::Uuid::new_v4()));
    let script = r#"
on run argv
  set outFile to POSIX file (item 1 of argv)
  try
    set imageData to the clipboard as «class PNGf»
    set fileRef to open for access outFile with write permission
    set eof fileRef to 0
    write imageData to fileRef
    close access fileRef
    return "image/png"
  on error
    try
      close access outFile
    end try
    return ""
  end try
end run
"#;
    let output = tokio::time::timeout(
        CLIPBOARD_COMMAND_TIMEOUT,
        tokio::process::Command::new("/usr/bin/osascript")
            .arg("-e")
            .arg(script)
            .arg(path.as_os_str())
            .output(),
    )
    .await
    .map_err(|_| "macOS clipboard read timed out".to_string())?
    .map_err(|error| format!("cannot run osascript: {error}"))?;
    if !output.status.success() || !path.exists() {
        let _ = tokio::fs::remove_file(&path).await;
        return Ok(None);
    }
    let metadata = tokio::fs::metadata(&path)
        .await
        .map_err(|error| format!("cannot inspect clipboard image: {error}"))?;
    if metadata.len() == 0 {
        let _ = tokio::fs::remove_file(&path).await;
        return Ok(None);
    }
    if metadata.len() as usize > MAX_CLIPBOARD_IMAGE_BYTES {
        let _ = tokio::fs::remove_file(&path).await;
        return Err(format!(
            "clipboard image is too large ({} MiB; limit {} MiB)",
            metadata.len() / (1024 * 1024),
            MAX_CLIPBOARD_IMAGE_BYTES / (1024 * 1024),
        ));
    }
    Ok(Some(PendingImage {
        path,
        media_type: "image/png".into(),
    }))
}

/// Read an image from the native clipboard without blocking Tokio's worker thread.
/// The temporary file is registered in App and removed after submit or exit.
async fn clipboard_image() -> std::result::Result<Option<PendingImage>, String> {
    #[cfg(target_os = "macos")]
    {
        return clipboard_image_macos().await;
    }
    let candidates = [
        ("wl-paste", vec!["--list-types"], vec!["--type"]),
        (
            "xclip",
            vec!["-selection", "clipboard", "-t", "TARGETS", "-o"],
            vec!["-selection", "clipboard", "-t"],
        ),
    ];

    for (program, list_args, mut read_args) in candidates {
        let Some(types) = clipboard_command(program, &list_args).await else {
            continue;
        };
        let Some((media_type, extension)) = advertised_image_type(&types) else {
            continue;
        };
        read_args.push(media_type);
        if program == "xclip" {
            read_args.push("-o");
        }
        let bytes = clipboard_command(program, &read_args)
            .await
            .ok_or_else(|| format!("{program} could not read the clipboard image"))?;
        if bytes.is_empty() {
            return Err(format!("{program} returned an empty clipboard image"));
        }
        if bytes.len() > MAX_CLIPBOARD_IMAGE_BYTES {
            return Err(format!(
                "clipboard image is too large ({} MiB; limit {} MiB)",
                bytes.len() / (1024 * 1024),
                MAX_CLIPBOARD_IMAGE_BYTES / (1024 * 1024),
            ));
        }

        let dir = std::env::temp_dir().join("gnomef-paste");
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
        let path = dir.join(format!("paste-{}.{}", uuid::Uuid::new_v4(), extension));
        tokio::fs::write(&path, bytes)
            .await
            .map_err(|error| format!("cannot save clipboard image: {error}"))?;
        return Ok(Some(PendingImage {
            path,
            media_type: media_type.to_string(),
        }));
    }

    Ok(None)
}

/// Send a desktop notification via `notify-send` (libnotify). Fire-and-forget:
/// a missing binary or a session without D-Bus just means no popup appears.
/// Falls back to the terminal bell so headless terminals still get a ping.
fn desktop_notify(title: &str, body: &str) {
    let ok = std::process::Command::new("notify-send")
        .arg("--app-name=GnomeAI")
        .arg("--expire-time=5000")
        .arg(title)
        .arg(body)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok();
    if !ok {
        // OSC 9 would be nicer, but the plain BEL works everywhere.
        print!("\x07");
    }
}

/// Render a per-turn token usage table plus session totals into the
/// transcript. Turns are listed most-recent-last so the table reads
/// chronologically top to bottom.
fn show_token_usage(app: &mut App) {
    if app.token_history.is_empty() {
        app.blocks.push(Block_::Note(
            "no completed turns yet this session — token counters are all zero".into(),
        ));
        return;
    }

    let mut lines = String::new();
    lines.push_str(" turn │      in │     out │   total │ duration\n");
    lines.push_str("──────┼─────────┼─────────┼─────────┼─────────\n");

    for (index, (input, output, ms)) in app.token_history.iter().enumerate() {
        lines.push_str(&format!(
            " {:>4} │ {:>7} │ {:>7} │ {:>7} │ {:>6.1}s\n",
            index + 1,
            input,
            output,
            input + output,
            *ms as f64 / 1000.0,
        ));
    }

    lines.push_str("──────┼─────────┼─────────┼─────────┼─────────\n");
    let turns = app.token_history.len();
    let total_ms: u64 = app.token_history.iter().map(|t| t.2).sum();
    let avg_in = app.tokens_in / turns.max(1) as i64;
    let avg_out = app.tokens_out / turns.max(1) as i64;
    lines.push_str(&format!(
        " total │ {:>7} │ {:>7} │ {:>7} │ {:>6.1}s\n",
        app.tokens_in,
        app.tokens_out,
        app.tokens_in + app.tokens_out,
        total_ms as f64 / 1000.0,
    ));
    lines.push_str(&format!(
        "\n{turns} turns · average {avg_in} in / {avg_out} out per turn · model: {}",
        app.model
    ));

    app.blocks.push(Block_::Note(lines));
}

fn export_conversation(app: &mut App) {
    use std::io::Write;

    let timestamp = chrono::Local::now().format("%Y%m%d_%H%M%S");
    let filename = format!("gnomeai_export_{}.md", timestamp);

    let mut content = String::new();
    content.push_str("# GnomeAI Conversation Export\n\n");
    content.push_str(&format!(
        "**Date:** {}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    ));
    content.push_str(&format!("**Provider:** {}\n", app.provider));
    content.push_str(&format!("**Model:** {}\n", app.model));
    content.push_str(&format!("**Workspace:** {}\n\n", app.workspace));
    content.push_str("---\n\n");

    for block in &app.blocks {
        match block {
            Block_::User(text) => {
                content.push_str("## User\n\n");
                content.push_str(text);
                content.push_str("\n\n");
            }
            Block_::Assistant(text) => {
                content.push_str("## Assistant\n\n");
                content.push_str(text);
                content.push_str("\n\n");
            }
            Block_::Reasoning(text) => {
                content.push_str("### Reasoning\n\n```\n");
                content.push_str(text);
                content.push_str("\n```\n\n");
            }
            Block_::Tool {
                name,
                summary,
                output,
                done,
                ok,
                ms,
            } => {
                content.push_str(&format!("### Tool: {}\n\n", name));
                content.push_str(&format!("**Summary:** {}\n\n", summary));
                content.push_str(&format!(
                    "**Status:** {} ({} ms)\n\n",
                    if *done {
                        if *ok { "✓ Success" } else { "✗ Failed" }
                    } else {
                        "⏳ Running"
                    },
                    ms
                ));
                content.push_str("```\n");
                content.push_str(output);
                content.push_str("\n```\n\n");
            }
            Block_::Diff(text) => {
                content.push_str("### Diff\n\n```diff\n");
                content.push_str(text);
                content.push_str("\n```\n\n");
            }
            Block_::Verify {
                stage,
                passed,
                summary,
            } => {
                content.push_str(&format!("### Verify: {}\n\n", stage));
                content.push_str(&format!(
                    "**Result:** {}\n\n",
                    if *passed { "✓ Passed" } else { "✗ Failed" }
                ));
                content.push_str(summary);
                content.push_str("\n\n");
            }
            Block_::Error(text) => {
                content.push_str("### Error\n\n```\n");
                content.push_str(text);
                content.push_str("\n```\n\n");
            }
            Block_::Note(text) => {
                content.push_str("> **Note:** ");
                content.push_str(text);
                content.push_str("\n\n");
            }
        }
    }

    match std::fs::File::create(&filename) {
        Ok(mut file) => match file.write_all(content.as_bytes()) {
            Ok(_) => {
                app.blocks.push(Block_::Note(format!(
                    "Conversation exported to: {}",
                    filename
                )));
            }
            Err(e) => {
                app.blocks
                    .push(Block_::Error(format!("Failed to write export file: {}", e)));
            }
        },
        Err(e) => {
            app.blocks.push(Block_::Error(format!(
                "Failed to create export file: {}",
                e
            )));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn rows_as_text<'a>(text: &'a str, wrapped: &WrappedComposer) -> Vec<&'a str> {
        wrapped.rows.iter().map(|row| &text[row.clone()]).collect()
    }

    #[test]
    fn a_long_line_without_newlines_grows_into_rows() {
        // The reported bug: the box was sized by counting `\n`, so this stayed
        // one row tall and everything after the first row fell outside it.
        let text = "x".repeat(45);
        let wrapped = wrap_composer(&text, text.len(), 20);
        assert_eq!(rows_as_text(&text, &wrapped).len(), 3);
    }

    #[test]
    fn the_caret_stays_inside_the_box_on_a_long_line() {
        let text = "x".repeat(45);
        let wrapped = wrap_composer(&text, text.len(), 20);
        assert!(
            wrapped.cursor_col < 20,
            "caret at column {} is drawn on the border",
            wrapped.cursor_col
        );
        assert_eq!(wrapped.cursor_row, 2);
        assert_eq!(wrapped.cursor_col, 5);
    }

    #[test]
    fn a_caret_at_a_full_row_boundary_moves_to_the_next_row() {
        let text = "x".repeat(20);
        let wrapped = wrap_composer(&text, text.len(), 20);
        assert_eq!((wrapped.cursor_row, wrapped.cursor_col), (1, 0));
        assert_eq!(
            wrapped.rows.len(),
            2,
            "the caret needs a row to sit on, so the box grows"
        );
    }

    #[test]
    fn wrapping_prefers_spaces_but_splits_an_oversized_word() {
        let text = "hello world";
        let wrapped = wrap_composer(text, 0, 6);
        assert_eq!(rows_as_text(text, &wrapped), vec!["hello ", "world"]);

        let long = "supercalifragilistic";
        let wrapped = wrap_composer(long, 0, 6);
        assert_eq!(rows_as_text(long, &wrapped)[0], "superc");
    }

    #[test]
    fn explicit_newlines_still_start_rows() {
        let text = "one\ntwo\n";
        let wrapped = wrap_composer(text, text.len(), 40);
        assert_eq!(rows_as_text(text, &wrapped), vec!["one", "two", ""]);
        assert_eq!((wrapped.cursor_row, wrapped.cursor_col), (2, 0));
    }

    #[test]
    fn every_byte_belongs_to_exactly_one_row() {
        // The caret maps a byte offset onto a row, so a gap or an overlap here
        // shows up as a caret that drifts as you type.
        for text in ["", "short", &"x".repeat(45), "hello world again", "a\n\nb"] {
            let wrapped = wrap_composer(text, 0, 7);
            assert_eq!(
                wrapped.rows[0].start, 0,
                "first row skips bytes in {text:?}"
            );
            for pair in wrapped.rows.windows(2) {
                let gap = pair[1].start - pair[0].end;
                // 0 within a hard line, 1 across the `\n` that was consumed.
                assert!(gap <= 1, "gap or overlap of {gap} bytes in {text:?}");
            }
            assert_eq!(
                wrapped.rows.last().unwrap().end,
                text.len(),
                "rows stop short of the end in {text:?}"
            );
        }
    }

    #[test]
    fn the_caret_is_reachable_at_every_offset() {
        let text = "hello world, o linie ceva mai lunga";
        for cursor in 0..=text.len() {
            if !text.is_char_boundary(cursor) {
                continue;
            }
            let wrapped = wrap_composer(text, cursor, 10);
            assert!(
                (wrapped.cursor_row as usize) < wrapped.rows.len(),
                "cursor {cursor} landed on row {} of {} rows",
                wrapped.cursor_row,
                wrapped.rows.len()
            );
            assert!(wrapped.cursor_col < 10);
        }
    }

    #[test]
    fn a_zero_width_composer_does_not_panic() {
        let wrapped = wrap_composer("abc", 3, 0);
        assert!(!wrapped.rows.is_empty());
    }

    struct Rendered {
        lines: Vec<String>,
        width: u16,
        height: u16,
        cursor: (u16, u16),
    }

    /// Renders the whole UI the way `run` does, so assertions look at what is
    /// actually on screen rather than at the layout maths behind it.
    fn render(app: &mut App, width: u16, height: u16) -> Rendered {
        let mut terminal = ratatui::Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let cursor = terminal.get_cursor_position().unwrap().into();
        let buffer = terminal.backend().buffer();
        let lines = (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();
        Rendered {
            lines,
            width,
            height,
            cursor,
        }
    }

    #[tokio::test]
    async fn typing_past_the_edge_grows_the_composer_instead_of_scrolling_sideways() {
        let (ops, _receiver) = mpsc::channel(8);
        let mut app = App::new();
        type_text(&mut app, &"x".repeat(45), &ops).await;

        let screen = render(&mut app, 24, 20);

        // 45 characters at 22 usable columns is three rows, all of which have
        // to be inside the border rather than clipped away to the first one.
        // Only rows inside the composer border count: the banner above it has
        // its own text. The last row holds a single character (22 + 22 + 1).
        let with_text = screen
            .lines
            .iter()
            .filter(|line| line.starts_with('│') && line.contains('x'))
            .count();
        assert_eq!(
            with_text,
            3,
            "composer drew {with_text} row(s) of text:\n{}",
            screen.lines.join("\n")
        );

        // And the caret is inside the box, not out on the border.
        let (cursor_x, cursor_y) = screen.cursor;
        assert!(
            cursor_x >= 1 && cursor_x < screen.width - 1,
            "caret column {cursor_x} is on the border"
        );
        assert!(
            cursor_y < screen.height - 1,
            "caret row {cursor_y} is on the border"
        );
    }

    #[tokio::test]
    async fn a_composer_taller_than_the_cap_scrolls_to_keep_the_caret_visible() {
        let (ops, _receiver) = mpsc::channel(8);
        let mut app = App::new();
        // Far more rows than COMPOSER_MAX_ROWS at this width.
        type_text(&mut app, &"y".repeat(24 * 15), &ops).await;

        let screen = render(&mut app, 24, 24);
        let (_, cursor_y) = screen.cursor;
        assert!(
            cursor_y < screen.height - 1,
            "caret at row {cursor_y} fell outside a capped composer"
        );
    }

    /// Type text into the composer through the real key handler.
    async fn type_text(app: &mut App, text: &str, ops: &mpsc::Sender<Op>) {
        for character in text.chars() {
            handle_key(app, key(KeyCode::Char(character)), ops).await;
        }
    }

    #[tokio::test]
    async fn slash_opens_the_full_command_menu() {
        let (ops, _receiver) = mpsc::channel(8);
        let mut app = App::new();

        type_text(&mut app, "/", &ops).await;
        assert_eq!(
            app.completions.len(),
            COMMANDS.len(),
            "a bare / must list every command"
        );
        assert_eq!(app.completion_idx, 0);
        assert_eq!(app.completions[0], "/help");

        // Typing narrows the same menu.
        type_text(&mut app, "me", &ops).await;
        assert_eq!(app.completions, vec!["/memory"]);

        // A space means the command is complete; the menu gets out of the way.
        type_text(&mut app, " ", &ops).await;
        assert!(app.completions.is_empty());
    }

    #[tokio::test]
    async fn arrows_navigate_the_menu_and_wrap() {
        let (ops, _receiver) = mpsc::channel(8);
        let mut app = App::new();
        type_text(&mut app, "/", &ops).await;
        let total = app.completions.len();

        handle_key(&mut app, key(KeyCode::Down), &ops).await;
        assert_eq!(app.completion_idx, 1);
        handle_key(&mut app, key(KeyCode::Up), &ops).await;
        assert_eq!(app.completion_idx, 0);

        // Up from the first entry wraps to the last, and back again.
        handle_key(&mut app, key(KeyCode::Up), &ops).await;
        assert_eq!(app.completion_idx, total - 1);
        handle_key(&mut app, key(KeyCode::Down), &ops).await;
        assert_eq!(app.completion_idx, 0);

        // Tab and Shift+Tab move the same selection.
        handle_key(&mut app, key(KeyCode::Tab), &ops).await;
        assert_eq!(app.completion_idx, 1);
        handle_key(&mut app, key(KeyCode::BackTab), &ops).await;
        assert_eq!(app.completion_idx, 0);
    }

    #[tokio::test]
    async fn tab_accepts_inline_command_autosuggestion_without_submitting() {
        let (ops, mut receiver) = mpsc::channel(8);
        let mut app = App::new();

        type_text(&mut app, "/mem", &ops).await;
        assert_eq!(command_autosuggestion(&app), Some("/memory"));
        assert_eq!(command_autosuggestion_suffix(&app), Some("ory"));

        handle_key(&mut app, key(KeyCode::Tab), &ops).await;

        assert_eq!(app.composer, "/memory ");
        assert!(app.completions.is_empty());
        assert!(receiver.try_recv().is_err(), "completion must not execute");
    }

    #[tokio::test]
    async fn provider_and_model_are_excluded_from_generic_autosuggestion() {
        let (ops, _receiver) = mpsc::channel(8);

        for command in ["/pro", "/mod"] {
            let mut app = App::new();
            type_text(&mut app, command, &ops).await;
            assert_eq!(app.completions.len(), 1);
            assert!(command_autosuggestion(&app).is_none());
            assert!(command_autosuggestion_suffix(&app).is_none());
        }

        let mut app = App::new();
        type_text(&mut app, "/mo", &ops).await;
        assert_eq!(command_autosuggestion(&app), Some("/mouse"));
    }

    #[tokio::test]
    async fn menu_scrolls_to_keep_the_selection_visible() {
        let (ops, _receiver) = mpsc::channel(8);
        let mut app = App::new();
        type_text(&mut app, "/", &ops).await;
        assert!(
            app.completions.len() > COMPLETION_ROWS,
            "this test needs more commands than fit on screen"
        );

        for _ in 0..COMPLETION_ROWS {
            handle_key(&mut app, key(KeyCode::Down), &ops).await;
        }
        assert_eq!(app.completion_idx, COMPLETION_ROWS);
        assert_eq!(app.completion_offset, 1, "window must follow the highlight");
        assert!(app.completion_idx < app.completion_offset + COMPLETION_ROWS);

        // Wrapping to the end scrolls the window to the bottom.
        handle_key(&mut app, key(KeyCode::Up), &ops).await;
        type_text(&mut app, "", &ops).await;
        app.completion_idx = 0;
        app.completion_offset = 0;
        handle_key(&mut app, key(KeyCode::Up), &ops).await;
        let total = app.completions.len();
        assert_eq!(app.completion_idx, total - 1);
        assert_eq!(app.completion_offset, total - COMPLETION_ROWS);
    }

    #[tokio::test]
    async fn enter_picks_the_highlighted_command() {
        let (ops, mut receiver) = mpsc::channel(8);
        let mut app = App::new();
        type_text(&mut app, "/", &ops).await;

        // Move to /new (second entry) and accept it.
        handle_key(&mut app, key(KeyCode::Down), &ops).await;
        assert_eq!(app.completions[app.completion_idx], "/new");
        handle_key(&mut app, key(KeyCode::Enter), &ops).await;
        assert_eq!(app.composer, "/new ");
        assert!(app.completions.is_empty());

        // A second Enter sends it.
        handle_key(&mut app, key(KeyCode::Enter), &ops).await;
        assert!(matches!(receiver.recv().await, Some(Op::NewSession)));
    }

    #[tokio::test]
    async fn escape_closes_the_menu_before_anything_else() {
        let (ops, _receiver) = mpsc::channel(8);
        let mut app = App::new();
        app.detached = true;
        app.scroll = 5;

        type_text(&mut app, "/", &ops).await;
        handle_key(&mut app, key(KeyCode::Esc), &ops).await;
        assert!(app.completions.is_empty());
        assert!(app.detached, "the first Esc only closes the menu");

        handle_key(&mut app, key(KeyCode::Esc), &ops).await;
        assert!(
            !app.detached,
            "the second Esc returns to the newest message"
        );
    }

    #[tokio::test]
    async fn arrows_still_reach_history_when_the_menu_is_closed() {
        let (ops, _receiver) = mpsc::channel(8);
        let mut app = App::new();
        app.history.push("mesaj anterior".into());

        handle_key(&mut app, key(KeyCode::Up), &ops).await;
        assert_eq!(app.composer, "mesaj anterior");
    }

    #[tokio::test]
    async fn help_lists_every_command_and_key() {
        let (ops, _receiver) = mpsc::channel(8);
        let mut app = App::new();
        submit(&mut app, "/help".into(), &ops).await;

        let Some(Block_::Note(text)) = app.blocks.last() else {
            panic!("/help must print a note");
        };
        for (name, description) in COMMANDS {
            assert!(text.contains(name), "missing {name}");
            assert!(text.contains(description), "missing description for {name}");
        }
        assert!(text.contains("Ctrl+C"), "key reference is missing");

        // The aliases behave the same.
        for alias in ["/?", "/commands"] {
            let mut app = App::new();
            submit(&mut app, alias.into(), &ops).await;
            assert!(
                matches!(app.blocks.last(), Some(Block_::Note(_))),
                "{alias}"
            );
        }
    }

    #[tokio::test]
    async fn provider_picker_masks_and_submits_api_key() {
        let (ops, mut receiver) = mpsc::channel(4);
        let mut app = App::new();
        submit(&mut app, "/provider".into(), &ops).await;
        assert!(app.provider_dialog.is_some());

        let action = handle_provider_key(&mut app, key(KeyCode::Enter), &ops).await;
        assert!(matches!(action, UiAction::None));
        assert!(matches!(
            app.provider_dialog.as_ref().unwrap().stage,
            ProviderStage::ApiKey
        ));

        for character in "sk-test".chars() {
            handle_provider_key(&mut app, key(KeyCode::Char(character)), &ops).await;
        }
        handle_provider_key(&mut app, key(KeyCode::Enter), &ops).await;

        let Some(Op::SetProvider {
            provider_id,
            api_key,
            base_url,
        }) = receiver.recv().await
        else {
            panic!("provider operation not emitted");
        };
        assert_eq!(provider_id, "openai");
        assert_eq!(api_key.unwrap().expose(), "sk-test");
        assert!(base_url.is_none());
        assert!(app.provider_dialog.is_none());
    }

    #[tokio::test]
    async fn provider_picker_allows_reusing_a_saved_key() {
        let (ops, mut receiver) = mpsc::channel(2);
        let mut app = App::new();
        submit(&mut app, "/provider".into(), &ops).await;

        handle_provider_key(&mut app, key(KeyCode::Enter), &ops).await;
        handle_provider_key(&mut app, key(KeyCode::Enter), &ops).await;

        assert!(matches!(
            receiver.recv().await,
            Some(Op::SetProvider {
                provider_id,
                api_key: None,
                ..
            }) if provider_id == "openai"
        ));
    }

    #[tokio::test]
    async fn exact_provider_command_opens_picker_on_first_enter() {
        let (ops, _receiver) = mpsc::channel(1);
        let mut app = App::new();

        for character in "/provider".chars() {
            handle_key(&mut app, key(KeyCode::Char(character)), &ops).await;
        }

        assert_eq!(app.completions, vec!["/provider"]);
        handle_key(&mut app, key(KeyCode::Enter), &ops).await;

        assert!(app.provider_dialog.is_some());
        assert!(app.composer.is_empty());
        assert!(app.completions.is_empty());
    }

    #[tokio::test]
    async fn provider_models_feed_the_searchable_model_picker() {
        let (ops, mut receiver) = mpsc::channel(4);
        let mut app = App::new();
        apply_event(
            &mut app,
            Event::ProviderChanged {
                provider: "DeepSeek".into(),
                model: "deepseek-v4-pro".into(),
                models: vec![
                    "deepseek-v4-pro".into(),
                    "deepseek-v4-flash".into(),
                    "deepseek-r1".into(),
                ],
            },
        );

        submit(&mut app, "/model".into(), &ops).await;
        assert!(app.model_dialog.is_some());
        for character in "flash".chars() {
            handle_model_key(&mut app, key(KeyCode::Char(character)), &ops).await;
        }
        handle_model_key(&mut app, key(KeyCode::Enter), &ops).await;

        assert_eq!(app.model, "deepseek-v4-flash");
        assert!(matches!(
            receiver.recv().await,
            Some(Op::SetModel { model }) if model == "deepseek-v4-flash"
        ));
        assert!(app.model_dialog.is_none());
    }

    #[tokio::test]
    async fn workspace_command_emits_a_core_workspace_change() {
        let (ops, mut receiver) = mpsc::channel(1);
        let mut app = App::new();

        submit(
            &mut app,
            r#"/workspace "/home/user/My Project""#.into(),
            &ops,
        )
        .await;

        let Some(Op::SetWorkspace { path }) = receiver.recv().await else {
            panic!("workspace operation not emitted");
        };
        assert_eq!(path, PathBuf::from("/home/user/My Project"));
        assert!(app.blocks.is_empty());
    }

    #[tokio::test]
    async fn romanian_workspace_intent_is_not_sent_to_the_model() {
        let (ops, mut receiver) = mpsc::channel(1);
        let mut app = App::new();

        submit(
            &mut app,
            "Vreau să schimb folderul în /home/gulas/Documents/gnomef-rs".into(),
            &ops,
        )
        .await;

        let Some(Op::SetWorkspace { path }) = receiver.recv().await else {
            panic!("workspace operation not emitted");
        };
        assert_eq!(path, PathBuf::from("/home/gulas/Documents/gnomef-rs"));
        assert!(app.blocks.is_empty());
    }

    #[tokio::test]
    async fn ordinary_file_path_reference_still_reaches_the_model() {
        let (ops, mut receiver) = mpsc::channel(1);
        let mut app = App::new();

        submit(
            &mut app,
            "Citește /home/gulas/project/README.md din proiect".into(),
            &ops,
        )
        .await;

        assert!(matches!(
            receiver.recv().await,
            Some(Op::Submit { text }) if text.contains("README.md")
        ));
    }

    #[test]
    fn image_markers_are_parsed_once_in_message_order() {
        assert_eq!(
            image_marker_paths(
                "first [image: /tmp/a.png] second [image: /tmp/b.jpg] [image: /tmp/a.png]"
            ),
            vec!["/tmp/a.png", "/tmp/b.jpg"]
        );
    }

    #[test]
    fn clipboard_prefers_png_and_accepts_common_image_types() {
        assert_eq!(
            advertised_image_type(b"text/plain\nimage/jpeg\nimage/png\n"),
            Some(("image/png", "png"))
        );
        assert_eq!(
            advertised_image_type(b"image/webp\n"),
            Some(("image/webp", "webp"))
        );
        assert_eq!(advertised_image_type(b"text/plain\n"), None);
    }

    #[tokio::test]
    async fn submit_sends_registered_clipboard_image_as_multipart() {
        let (ops, mut receiver) = mpsc::channel(1);
        let mut app = App::new();
        let root =
            std::env::temp_dir().join(format!("gnomeai-tui-image-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("paste.png");
        std::fs::write(&path, b"\x89PNG\r\n\x1a\n").unwrap();
        let key = path.display().to_string();
        app.pending_images.insert(
            key.clone(),
            PendingImage {
                path: path.clone(),
                media_type: "image/png".into(),
            },
        );

        submit(&mut app, format!("describe this [image: {key}]"), &ops).await;

        let Some(Op::Submit { text }) = receiver.recv().await else {
            panic!("multipart submit was not emitted");
        };
        let parts: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "describe this");
        assert_eq!(parts[1]["type"], "image_url");
        assert!(
            parts[1]["image_url"]["url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
        assert!(
            !path.exists(),
            "temporary image must be removed after submit"
        );
        assert!(
            app.blocks
                .iter()
                .any(|block| matches!(block, Block_::User(text) if text.contains("📎")))
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mouse_wheel_detaches_and_returns_to_the_transcript_tail() {
        let mut app = App::new();
        app.max_scroll = 6;
        app.scroll = app.max_scroll;
        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 1,
                row: 1,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(app.scroll, 3);
        assert!(app.detached);

        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 1,
                row: 1,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(app.scroll, 6);
        assert!(!app.detached);
    }

    #[test]
    fn wheel_extends_an_active_mouse_selection_while_scrolling() {
        let mut app = App::new();
        app.transcript_area = Rect::new(0, 0, 40, 5);
        app.max_scroll = 20;
        app.scroll = 10;
        app.detached = true;

        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 4,
                row: 2,
                modifiers: KeyModifiers::NONE,
            },
        );
        handle_mouse(
            &mut app,
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 4,
                row: 2,
                modifiers: KeyModifiers::NONE,
            },
        );

        let selection = app.selection.expect("selection should remain active");
        assert_eq!(selection.anchor, TextPoint { row: 12, column: 4 });
        assert_eq!(selection.focus, TextPoint { row: 9, column: 4 });
        assert!(selection.dragging);
        assert_eq!(app.scroll, 7);
    }

    #[test]
    fn selected_text_uses_the_same_wrapped_transcript_rows() {
        let mut app = App::new();
        app.transcript_area = Rect::new(0, 0, 40, 8);
        let first_assistant_row = Paragraph::new(transcript_lines(&app))
            .wrap(Wrap { trim: false })
            .line_count(app.transcript_area.width) as u16;
        app.blocks
            .push(Block_::Assistant("alpha\nbeta\ngamma".into()));
        app.selection = Some(TextSelection {
            anchor: TextPoint {
                row: first_assistant_row,
                column: 0,
            },
            focus: TextPoint {
                row: first_assistant_row + 1,
                column: 3,
            },
            dragging: false,
        });

        assert_eq!(selected_text(&app).as_deref(), Some("alpha\nbeta"));
    }

    #[test]
    fn transcript_scroll_follows_wrapped_streaming_output() {
        let paragraph = Paragraph::new(
            "This is one logical line, but it occupies several terminal rows while it streams.",
        )
        .wrap(Wrap { trim: false });
        let area = Rect::new(0, 0, 16, 2);
        let rendered_rows = paragraph.line_count(area.width);

        assert!(rendered_rows > 2);
        assert_eq!(
            transcript_scroll(&paragraph, area, 0, false),
            (rendered_rows - usize::from(area.height)) as u16
        );
    }

    #[test]
    fn transcript_scroll_preserves_manual_view_while_output_grows() {
        let before = Paragraph::new(
            "This is another long logical line that wraps across several terminal rows.",
        )
        .wrap(Wrap { trim: false });
        let after = Paragraph::new(
            "This is another long logical line that wraps across several terminal rows. \
             New streaming output must not pull a detached reader toward the bottom.",
        )
        .wrap(Wrap { trim: false });
        let area = Rect::new(0, 0, 14, 2);
        let tail_scroll = transcript_scroll(&before, area, 0, false);
        let manual_top = tail_scroll.saturating_sub(2);

        assert!(tail_scroll >= 2);
        assert!(transcript_max_scroll(&after, area) > tail_scroll);
        assert_eq!(
            transcript_scroll(&after, area, manual_top, true),
            manual_top
        );
    }

    #[test]
    fn scroll_percent_tracks_position() {
        assert_eq!(scroll_percent(0, 0), 100);
        assert_eq!(scroll_percent(0, 200), 0);
        assert_eq!(scroll_percent(100, 200), 50);
        assert_eq!(scroll_percent(200, 200), 100);
    }

    #[test]
    fn very_long_conversations_keep_scroll_math_sane() {
        // A transcript far taller than u16::MAX rendered rows must clamp
        // instead of overflowing.
        let text = "line\n".repeat(70_000);
        let paragraph = Paragraph::new(text).wrap(Wrap { trim: false });
        let area = Rect::new(0, 0, 80, 20);
        let max_scroll = transcript_max_scroll(&paragraph, area);
        assert_eq!(max_scroll, u16::MAX);
        assert_eq!(transcript_scroll(&paragraph, area, 0, false), u16::MAX);
        // A detached reader keeps their place regardless of transcript size.
        assert_eq!(transcript_scroll(&paragraph, area, 1234, true), 1234);
        assert_eq!(scroll_percent(1234, max_scroll), 1);
    }

    #[test]
    fn streaming_does_not_move_a_detached_reader() {
        let mut app = App::new();
        app.max_scroll = 50;
        app.scroll = 50;
        scroll_up(&mut app, 20);
        assert!(app.detached);
        let before = app.scroll;

        // New tokens arrive while the user reads history.
        for _ in 0..100 {
            apply_event(
                &mut app,
                Event::Token {
                    text: "more streamed output ".into(),
                },
            );
        }
        assert_eq!(app.scroll, before);
        assert!(app.detached);

        scroll_to_bottom(&mut app);
        assert!(!app.detached);
    }

    #[tokio::test]
    async fn mouse_command_requests_capture_toggle() {
        let (ops, _receiver) = mpsc::channel(1);
        let mut app = App::new();
        assert!(app.mouse_enabled);

        submit(&mut app, "/mouse".into(), &ops).await;
        assert_eq!(app.mouse_toggle, Some(false));

        app.mouse_toggle = None;
        submit(&mut app, "/mouse on".into(), &ops).await;
        assert_eq!(app.mouse_toggle, Some(true));

        app.mouse_toggle = None;
        submit(&mut app, "/mouse sideways".into(), &ops).await;
        assert!(app.mouse_toggle.is_none());
        assert!(matches!(app.blocks.last(), Some(Block_::Error(_))));
    }

    #[tokio::test]
    async fn contrast_command_toggles_high_contrast_mode() {
        let (ops, _receiver) = mpsc::channel(1);
        let mut app = App::new();
        assert!(!app.high_contrast);

        submit(&mut app, "/contrast".into(), &ops).await;
        assert!(app.high_contrast);
        assert!(matches!(app.blocks.last(), Some(Block_::Note(_))));

        submit(&mut app, "/contrast".into(), &ops).await;
        assert!(!app.high_contrast);
    }

    #[tokio::test]
    async fn notify_command_toggles_notifications() {
        let (ops, _receiver) = mpsc::channel(1);
        let mut app = App::new();
        assert!(app.notifications_enabled, "default should be on");

        submit(&mut app, "/notify".into(), &ops).await;
        assert!(!app.notifications_enabled);
        assert!(matches!(app.blocks.last(), Some(Block_::Note(_))));

        submit(&mut app, "/notify on".into(), &ops).await;
        assert!(app.notifications_enabled);

        submit(&mut app, "/notify sideways".into(), &ops).await;
        assert!(matches!(app.blocks.last(), Some(Block_::Error(_))));
        assert!(app.notifications_enabled, "bad arg must not flip the flag");
    }

    #[tokio::test]
    async fn tokens_command_reports_usage_table() {
        let (ops, _receiver) = mpsc::channel(1);
        let mut app = App::new();

        // Empty session: explanatory note, no table.
        submit(&mut app, "/tokens".into(), &ops).await;
        match app.blocks.last() {
            Some(Block_::Note(text)) => assert!(text.contains("no completed turns")),
            other => panic!("expected Note, got {other:?}"),
        }

        // Simulate two completed turns.
        for (input, output, ms) in [(100_i64, 20_i64, 1500_u64), (300, 80, 2500)] {
            apply_event(
                &mut app,
                Event::TurnCompleted {
                    turn_id: 1,
                    input_tokens: input,
                    output_tokens: output,
                    duration_ms: ms,
                },
            );
        }
        assert_eq!(app.tokens_in, 400);
        assert_eq!(app.tokens_out, 100);
        assert_eq!(app.token_history.len(), 2);

        submit(&mut app, "/tokens".into(), &ops).await;
        match app.blocks.last() {
            Some(Block_::Note(text)) => {
                assert!(
                    text.contains("  400"),
                    "totals row must show 400 in: {text}"
                );
                assert!(
                    text.contains("  100"),
                    "totals row must show 100 out: {text}"
                );
                assert!(text.contains("2 turns"), "summary line: {text}");
            }
            other => panic!("expected Note, got {other:?}"),
        }
    }

    #[test]
    fn osc52_encodes_clipboard_payload() {
        let sequence = osc52_sequence("salut");
        assert!(sequence.starts_with("\x1b]52;c;"));
        assert!(sequence.ends_with('\x07'));
        assert!(sequence.contains("c2FsdXQ="));
    }

    #[test]
    fn copy_finds_the_last_assistant_reply() {
        let mut app = App::new();
        assert!(last_assistant_text(&app).is_none());
        app.blocks.push(Block_::Assistant("first".into()));
        app.blocks.push(Block_::User("question".into()));
        app.blocks.push(Block_::Assistant("second".into()));
        app.blocks.push(Block_::Note("note".into()));
        assert_eq!(last_assistant_text(&app).as_deref(), Some("second"));
    }

    #[tokio::test]
    async fn memory_commands_map_to_ops() {
        let (ops, mut receiver) = mpsc::channel(8);
        let mut app = App::new();

        submit(&mut app, "/memory".into(), &ops).await;
        assert!(matches!(receiver.recv().await, Some(Op::MemoryShow)));
        submit(&mut app, "/memory clear".into(), &ops).await;
        assert!(matches!(receiver.recv().await, Some(Op::MemoryClear)));
        submit(&mut app, "/memory off".into(), &ops).await;
        assert!(matches!(
            receiver.recv().await,
            Some(Op::MemorySet { enabled: false })
        ));
        submit(&mut app, "/memory status".into(), &ops).await;
        assert!(matches!(receiver.recv().await, Some(Op::MemoryStatus)));
        submit(&mut app, "/memory dream".into(), &ops).await;
        assert!(matches!(
            receiver.recv().await,
            Some(Op::MemoryDream { dry_run: false })
        ));
        submit(&mut app, "/memory dream --dry-run".into(), &ops).await;
        assert!(matches!(
            receiver.recv().await,
            Some(Op::MemoryDream { dry_run: true })
        ));
        submit(&mut app, "/memory reindex".into(), &ops).await;
        assert!(matches!(receiver.recv().await, Some(Op::MemoryReindex)));
        submit(&mut app, "/memory forget mem_abc".into(), &ops).await;
        let Some(Op::MemoryForget { id }) = receiver.recv().await else {
            panic!("forget operation not emitted");
        };
        assert_eq!(id, "mem_abc");
    }

    #[tokio::test]
    async fn workspace_number_uses_the_recent_list() {
        let (ops, mut receiver) = mpsc::channel(1);
        let mut app = App::new();
        app.recent_workspaces = vec!["/projects/alpha".into(), "/projects/beta".into()];

        submit(&mut app, "/workspace 2".into(), &ops).await;
        let Some(Op::SetWorkspace { path }) = receiver.recv().await else {
            panic!("workspace operation not emitted");
        };
        assert_eq!(path, PathBuf::from("/projects/beta"));
    }

    #[tokio::test]
    async fn session_dialog_resumes_renames_and_deletes() {
        let (ops, mut receiver) = mpsc::channel(8);
        let mut app = App::new();

        submit(&mut app, "/sessions".into(), &ops).await;
        assert!(matches!(receiver.recv().await, Some(Op::ListSessions)));

        let summary = |id: &str| SessionSummary {
            id: id.into(),
            title: None,
            workspace: PathBuf::from("/projects/alpha"),
            model: "m".into(),
            updated_at: 0,
            turns: 3,
            is_current: false,
        };
        apply_event(
            &mut app,
            Event::SessionList {
                sessions: vec![summary("aaa"), summary("bbb")],
            },
        );
        assert!(app.session_dialog.is_some());

        // Delete the second entry.
        handle_session_key(&mut app, key(KeyCode::Down), &ops).await;
        handle_session_key(&mut app, key(KeyCode::Char('d')), &ops).await;
        assert!(matches!(
            receiver.recv().await,
            Some(Op::DeleteSession { id }) if id == "bbb"
        ));

        // Rename it through the inline input.
        handle_session_key(&mut app, key(KeyCode::Char('r')), &ops).await;
        for character in "fix".chars() {
            handle_session_key(&mut app, key(KeyCode::Char(character)), &ops).await;
        }
        handle_session_key(&mut app, key(KeyCode::Enter), &ops).await;
        assert!(matches!(
            receiver.recv().await,
            Some(Op::RenameSession { id, title }) if id == "bbb" && title == "fix"
        ));

        // Resume closes the dialog.
        handle_session_key(&mut app, key(KeyCode::Up), &ops).await;
        handle_session_key(&mut app, key(KeyCode::Enter), &ops).await;
        assert!(matches!(
            receiver.recv().await,
            Some(Op::ResumeSession { id }) if id == "aaa"
        ));
        assert!(app.session_dialog.is_none());
    }

    #[tokio::test]
    async fn account_choice_requests_official_login_flow() {
        let (ops, _receiver) = mpsc::channel(1);
        let mut app = App::new();
        app.provider_dialog = Some(ProviderDialog {
            selected: PROVIDERS
                .iter()
                .position(|provider| provider.id == "anthropic-account")
                .unwrap(),
            stage: ProviderStage::Select,
            input: String::new(),
            base_url: None,
            error: None,
        });

        let action = handle_provider_key(&mut app, key(KeyCode::Enter), &ops).await;
        match action {
            UiAction::Authenticate { provider_id, flow } => {
                assert_eq!(provider_id, "anthropic-account");
                assert!(matches!(flow, AccountLogin::ClaudeCode));
            }
            UiAction::None => panic!("login action not emitted"),
        }
    }
}
