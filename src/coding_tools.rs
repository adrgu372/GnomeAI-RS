//! The coding tools the model calls, plus the system prompt that tells it
//! how to use them.
//!
//! Tool descriptions are prompt engineering, not documentation. The model
//! learns the patch format from `ApplyPatchTool::definition()` and nowhere else, so
//! that string is load-bearing — treat a change to it like a change to code.
//!
//! Output sizing is centralised: the model sees a bounded head/tail preview and
//! can retrieve the complete stored result with `read_tool_output`.

use anyhow::Result;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::apply_patch;
use crate::config::AppConfig;
use crate::desktop;
use crate::firecrawl::{firecrawl_fetch, firecrawl_search};
use crate::node_protocol::QueueJobRequest;
use crate::nodes;
use crate::privilege::{PrivilegeBroker, command_requests_privilege};
use crate::provider::ToolSpec;
use crate::sandbox::{SandboxMode, SandboxPolicy, spawn_sandboxed_with_cancel};
use crate::skills;
use crate::tooling::{FilePatch, Registry, Tool, ToolDefinition, ToolOutcome, ToolOutputStore};

const MAX_TOOL_OUTPUT: usize = 16 * 1024;
const MAX_READ_LINES: usize = 800;
const MAX_SEARCH_HITS: usize = 60;
const MAX_LIST_ENTRIES: usize = 500;

// ---------------------------------------------------------------------------
// System prompt
// ---------------------------------------------------------------------------

/// Cheapest large win available: read the project's own instructions and put
/// them in front of the model. `AGENTS.md` is the convention Codex, Hermes and
/// most others already follow, so repositories in the wild often have one.
pub fn build_system_prompt(root: &Path) -> String {
    let mut s = String::from(
        "You are a coding agent working inside a single repository.\n\
         \n\
         Work in small steps. Read before you edit: never patch a file you have\n\
         not read in this session. Use `list_files` and prefer `search` over\n\
         guessing at paths.\n\
         \n\
         Edits go through `apply_patch`. Never rewrite a whole file to change a\n\
         few lines, and never use `shell` with a heredoc to write source files.\n\
         \n\
         After editing, the harness runs the project's build and tests and gives\n\
         you the result. Treat that result as ground truth, including when it\n\
         contradicts you. If the same error survives two attempts, stop and\n\
         explain what you think is wrong rather than trying a third variation.\n\
         \n\
         Command access follows the user-visible execution mode. `read-only`\n\
         is isolated; `normal` has ordinary OS access after approval; and\n\
         `full-access` has ordinary OS access without approval. Never claim a\n\
         command is sandboxed unless the active mode actually says so.\n\
         Root access is separate from full-access. Use the `sudo` tool for a\n\
         command that genuinely needs root; never put `sudo` inside `shell`.\n\
         The interface collects authentication locally and the model never\n\
         receives or sees the password.\n\
         For graphical tasks, use the `desktop` tool autonomously: observe the\n\
         current screen, inspect the attached screenshot, perform one precise\n\
         action, and inspect the returned screenshot before continuing. Never\n\
         guess coordinates without a current capture.\n\
         In `read-only`, `web_search` and `web_fetch` are the only\n\
         network-capable tools; they work only while the user-visible Web\n\
         Search switch is enabled.\n\
         Use `node` to list paired weak devices and to run work on one of\n\
         them. Inspect the list before choosing a node. Remote root is a\n\
         separate, centrally controlled permission and never follows ordinary\n\
         full-access automatically.\n\
         Use `learn_skill` only when the user explicitly asks to retain a\n\
         reusable workflow. Learning stores instructions and an optional POSIX\n\
         shell entrypoint; it never runs it. Use `run_skill` as a separate\n\
         approved call when the user asks to execute one.\n",
    );

    for name in ["AGENTS.md", "CLAUDE.md", ".agentrc"] {
        let path = root.join(name);
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        // Truncate: a project that wrote a 200 KB instruction file should not
        // get to eat the whole context window.
        let content: String = content.chars().take(8000).collect();
        s.push_str(&format!(
            "\n\n--- {name} (project instructions) ---\n{content}\n"
        ));
        break;
    }

    s.push_str(&skills::catalog_prompt(root));
    s
}

fn cap(s: &str, limit: usize) -> String {
    if s.len() <= limit {
        return s.to_string();
    }
    // Keep the head and the tail: compiler errors live at the top, test
    // failures and summaries at the bottom. The middle is almost always noise.
    let head: String = s.chars().take(limit * 2 / 3).collect();
    let tail: String = s
        .chars()
        .rev()
        .take(limit / 3)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!(
        "{head}\n\n[... {} bytes elided ...]\n\n{tail}",
        s.len() - limit
    )
}

// ---------------------------------------------------------------------------
// shell
// ---------------------------------------------------------------------------

pub struct ShellTool {
    pub policy: SandboxPolicy,
}

#[async_trait::async_trait]
impl Tool for ShellTool {
    fn definition(&self) -> ToolDefinition {
        let access = match self.policy.mode {
            SandboxMode::ReadOnly | SandboxMode::IsolatedWorkspaceWrite => {
                "The command is isolated: no network and no writes outside approved scratch paths."
            }
            SandboxMode::Normal => {
                "After user approval, the command has the user's normal OS, filesystem and network access."
            }
            SandboxMode::FullAccess => {
                "The command has unrestricted user-level OS, filesystem and network access without approval."
            }
        };
        ToolDefinition::user_process(ToolSpec {
            name: "shell".into(),
            description: format!(
                "Run a shell command with the current workspace as its initial directory. \
                 {access} Use this for builds, tests and inspection — not for editing source \
                 files, which go through apply_patch."
            ),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The command line to run, e.g. `cargo test --lib`"
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "description": "Optional. Defaults to 120000."
                    }
                },
                "required": ["command"]
            }),
        })
    }

    async fn call(&self, args: Value, cancel: &CancellationToken) -> Result<ToolOutcome> {
        let Some(command) = args["command"].as_str() else {
            return Ok(ToolOutcome {
                content: "missing `command`".into(),
                ok: false,
                touched: vec![],
                patches: vec![],
            });
        };
        if command_requests_privilege(command) {
            return Ok(failed_outcome(
                "sudo is blocked inside `shell`; call the dedicated `sudo` tool so the local interface can authenticate without exposing the password",
            ));
        }

        let mut policy = self.policy.clone();
        if let Some(t) = args["timeout_ms"].as_u64() {
            policy.timeout_ms = t.min(600_000);
        }

        // Go through a shell so pipes and redirection work. The sandbox is what
        // makes this safe, not argument parsing.
        let out = spawn_sandboxed_with_cancel(
            &policy,
            "/bin/sh",
            &["-c".into(), command.to_string()],
            cancel,
        )
        .await?;

        let mut body = String::new();
        if !out.stdout.is_empty() {
            body.push_str(&out.stdout);
        }
        if !out.stderr.is_empty() {
            body.push_str("\n--- stderr ---\n");
            body.push_str(&out.stderr);
        }
        if out.timed_out {
            body.push_str("\n[command timed out]");
        }
        if out.cancelled {
            body.push_str("\n[command interrupted]");
        }
        if body.trim().is_empty() {
            body = "(no output)".into();
        }

        let code = out.exit_code.unwrap_or(-1);
        Ok(ToolOutcome {
            content: format!("exit {code}\n\n{body}"),
            ok: code == 0 && !out.timed_out && !out.cancelled,
            // Conservative: assume any command may have written something, so
            // verification runs. Cheaper than missing a `sed -i`.
            touched: if policy.mode == SandboxMode::ReadOnly {
                vec![]
            } else {
                vec![policy.cwd.clone()]
            },
            patches: vec![],
        })
    }
}

// ---------------------------------------------------------------------------
// apply_patch
// ---------------------------------------------------------------------------

pub struct ApplyPatchTool {
    pub root: PathBuf,
    pub writable: bool,
}

#[async_trait::async_trait]
impl Tool for ApplyPatchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::workspace_write(ToolSpec {
            name: "apply_patch".into(),
            // This description IS the format specification. The model has no
            // other source for it.
            description: "Edit files with a context patch. Locate changes by surrounding \
                 context, never by line number.\n\
                 \n\
                 Format:\n\
                 *** Begin Patch\n\
                 *** Update File: path/to/file.rs\n\
                 @@ optional marker such as an enclosing function signature\n\
                 [space]unchanged context line\n\
                 -removed line\n\
                 +added line\n\
                 [space]unchanged context line\n\
                 *** Add File: path/to/new.rs\n\
                 +every line of the new file, each prefixed with +\n\
                 *** Delete File: path/to/dead.rs\n\
                 *** End Patch\n\
                 \n\
                 Include at least three lines of unchanged context on each side of a \
                 change. If the context is not unique in the file the patch is rejected — \
                 add more context or an @@ marker rather than retrying the same text. \
                 Paths are relative to the workspace root; `..` and .git are refused."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "patch": { "type": "string", "description": "The full patch text." }
                },
                "required": ["patch"]
            }),
        })
    }

    async fn call(&self, args: Value, _cancel: &CancellationToken) -> Result<ToolOutcome> {
        if !self.writable {
            return Ok(ToolOutcome {
                content: "apply_patch is disabled by the read-only sandbox policy".into(),
                ok: false,
                touched: vec![],
                patches: vec![],
            });
        }
        let Some(text) = args["patch"].as_str() else {
            return Ok(ToolOutcome {
                content: "missing `patch`".into(),
                ok: false,
                touched: vec![],
                patches: vec![],
            });
        };

        let parsed = match apply_patch::parse(text) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolOutcome {
                    content: format!("patch did not parse: {e}"),
                    ok: false,
                    touched: vec![],
                    patches: vec![],
                });
            }
        };

        match apply_patch::apply(&self.root, &parsed) {
            Ok(applied) => {
                let patches = applied
                    .changes
                    .into_iter()
                    .map(|change| FilePatch {
                        path: change.path,
                        before: change.before,
                        after: change.after,
                        diff: change.diff,
                    })
                    .collect();
                Ok(ToolOutcome {
                    // Hand back the resulting diff, not "ok". The model needs
                    // to see what landed to stay in sync with the disk.
                    content: format!(
                        "applied to {} file(s):\n\n{}",
                        applied.files_changed.len(),
                        cap(&applied.diff, MAX_TOOL_OUTPUT)
                    ),
                    ok: true,
                    touched: applied.files_changed,
                    patches,
                })
            }
            Err(e) => Ok(ToolOutcome {
                content: format!("patch failed, nothing was written: {e}"),
                ok: false,
                touched: vec![],
                patches: vec![],
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// read_file
// ---------------------------------------------------------------------------

pub struct ReadFileTool {
    pub root: PathBuf,
}

#[async_trait::async_trait]
impl Tool for ReadFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::workspace_read(ToolSpec {
            name: "read_file".into(),
            description: "Read a file from the workspace. Returns numbered lines. \
                 Use offset and limit for large files rather than reading everything."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path":   { "type": "string" },
                    "offset": { "type": "integer", "description": "First line, 1-based." },
                    "limit":  { "type": "integer", "description": "Max lines. Default 800." }
                },
                "required": ["path"]
            }),
        })
    }

    async fn call(&self, args: Value, _cancel: &CancellationToken) -> Result<ToolOutcome> {
        let rel = PathBuf::from(args["path"].as_str().unwrap_or_default());
        let abs = match apply_patch::resolve_path(&self.root, &rel) {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolOutcome {
                    content: e.to_string(),
                    ok: false,
                    touched: vec![],
                    patches: vec![],
                });
            }
        };

        let bytes = match std::fs::read(&abs) {
            Ok(b) => b,
            Err(e) => {
                return Ok(ToolOutcome {
                    content: format!("cannot read {}: {e}", rel.display()),
                    ok: false,
                    touched: vec![],
                    patches: vec![],
                });
            }
        };

        // A NUL in the first block means binary. Feeding that to a model is
        // pure token burn.
        if bytes.iter().take(8192).any(|b| *b == 0) {
            return Ok(ToolOutcome {
                content: format!("{} is binary ({} bytes)", rel.display(), bytes.len()),
                ok: false,
                touched: vec![],
                patches: vec![],
            });
        }

        let text = String::from_utf8_lossy(&bytes);
        let offset = args["offset"].as_u64().unwrap_or(1).max(1) as usize;
        let limit = args["limit"].as_u64().unwrap_or(MAX_READ_LINES as u64) as usize;
        let limit = limit.min(MAX_READ_LINES);

        let all: Vec<&str> = text.lines().collect();
        let end = (offset - 1 + limit).min(all.len());
        let slice = &all[(offset - 1).min(all.len())..end];

        let mut body = String::new();
        for (i, line) in slice.iter().enumerate() {
            body.push_str(&format!("{:>6}  {line}\n", offset + i));
        }
        if end < all.len() {
            body.push_str(&format!("\n[{} more lines]\n", all.len() - end));
        }

        Ok(ToolOutcome {
            content: cap(&body, MAX_TOOL_OUTPUT),
            ok: true,
            touched: vec![],
            patches: vec![],
        })
    }
}

// ---------------------------------------------------------------------------
// search
// ---------------------------------------------------------------------------

pub struct SearchTool {
    pub root: PathBuf,
}

#[async_trait::async_trait]
impl Tool for SearchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::workspace_read(ToolSpec {
            name: "search".into(),
            description: "Regex search across the workspace, respecting .gitignore. \
                 This is how you find code — prefer it over guessing at file paths."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Rust regex syntax." },
                    "glob":    { "type": "string", "description": "Optional filename filter, e.g. *.rs" }
                },
                "required": ["pattern"]
            }),
        })
    }

    async fn call(&self, args: Value, _cancel: &CancellationToken) -> Result<ToolOutcome> {
        let pattern = args["pattern"].as_str().unwrap_or_default().to_string();
        let glob = args["glob"].as_str().map(str::to_string);
        let root = self.root.clone();

        // Walking a large tree is blocking work; keep it off the async runtime.
        let result =
            tokio::task::spawn_blocking(move || search_blocking(&root, &pattern, glob)).await??;

        Ok(ToolOutcome {
            content: cap(&result, MAX_TOOL_OUTPUT),
            ok: true,
            touched: vec![],
            patches: vec![],
        })
    }
}

fn search_blocking(root: &Path, pattern: &str, glob: Option<String>) -> Result<String> {
    let re = match regex::Regex::new(pattern) {
        Ok(r) => r,
        Err(e) => return Ok(format!("invalid regex: {e}")),
    };

    let mut out = String::new();
    let mut hits = 0usize;

    // `ignore` gives .gitignore handling for free, which is the difference
    // between searching a repository and searching node_modules.
    for entry in ignore::WalkBuilder::new(root).hidden(false).build() {
        if hits >= MAX_SEARCH_HITS {
            out.push_str("\n[more matches suppressed — narrow the pattern]\n");
            break;
        }
        let Ok(entry) = entry else { continue };
        if !entry.file_type().map_or(false, |t| t.is_file()) {
            continue;
        }

        let path = entry.path();
        if let Some(g) = &glob {
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if !glob_match(g, &name) {
                continue;
            }
        }

        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        if bytes.iter().take(4096).any(|b| *b == 0) {
            continue;
        }

        let text = String::from_utf8_lossy(&bytes);
        let rel = path.strip_prefix(root).unwrap_or(path);

        for (n, line) in text.lines().enumerate() {
            if !re.is_match(line) {
                continue;
            }
            out.push_str(&format!(
                "{}:{}: {}\n",
                rel.display(),
                n + 1,
                line.trim().chars().take(200).collect::<String>()
            ));
            hits += 1;
            if hits >= MAX_SEARCH_HITS {
                break;
            }
        }
    }

    if out.is_empty() {
        out.push_str("no matches");
    }
    Ok(out)
}

/// Enough for `*.rs` and `Cargo.*`. Pull in the `globset` crate if you ever
/// need real glob semantics.
fn glob_match(pattern: &str, name: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == name,
        Some((pre, post)) => name.starts_with(pre) && name.ends_with(post),
    }
}

// ---------------------------------------------------------------------------
// list_files
// ---------------------------------------------------------------------------

pub struct ListFilesTool {
    pub root: PathBuf,
}

#[async_trait::async_trait]
impl Tool for ListFilesTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::workspace_read(ToolSpec {
            name: "list_files".into(),
            description: "List files and directories in the workspace while respecting \
                          .gitignore. Use this before guessing repository structure."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Workspace-relative directory. Defaults to the workspace root."
                    },
                    "depth": {
                        "type": "integer",
                        "description": "Maximum recursive depth, from 1 to 8. Defaults to 2."
                    }
                }
            }),
        })
    }

    async fn call(&self, args: Value, _cancel: &CancellationToken) -> Result<ToolOutcome> {
        let root = self.root.clone();
        let requested = args["path"].as_str().unwrap_or(".").to_string();
        let depth = args["depth"].as_u64().unwrap_or(2).clamp(1, 8) as usize;
        let result =
            tokio::task::spawn_blocking(move || list_files_blocking(&root, &requested, depth))
                .await??;
        Ok(ToolOutcome {
            content: cap(&result, MAX_TOOL_OUTPUT),
            ok: true,
            touched: vec![],
            patches: vec![],
        })
    }
}

fn list_files_blocking(root: &Path, requested: &str, depth: usize) -> Result<String> {
    let root = root.canonicalize()?;
    let start = root.join(requested);
    let start = start.canonicalize()?;
    if !start.starts_with(&root) {
        anyhow::bail!("path escapes the workspace");
    }
    if !start.is_dir() {
        anyhow::bail!("not a directory: {}", start.display());
    }

    let mut entries = Vec::new();
    for entry in ignore::WalkBuilder::new(&start)
        .hidden(false)
        .max_depth(Some(depth))
        .build()
    {
        let Ok(entry) = entry else { continue };
        if entry.path() == start {
            continue;
        }
        let rel = entry.path().strip_prefix(&root).unwrap_or(entry.path());
        let suffix = if entry.file_type().is_some_and(|kind| kind.is_dir()) {
            "/"
        } else {
            ""
        };
        entries.push(format!("{}{suffix}", rel.display()));
        if entries.len() >= MAX_LIST_ENTRIES {
            entries.push("[more entries suppressed — narrow path or depth]".into());
            break;
        }
    }
    entries.sort();
    if entries.is_empty() {
        Ok("(empty directory)".into())
    } else {
        Ok(entries.join("\n"))
    }
}

// ---------------------------------------------------------------------------
// web_search / web_fetch
// ---------------------------------------------------------------------------

pub struct WebSearchTool {
    pub config: Arc<RwLock<AppConfig>>,
}

#[async_trait::async_trait]
impl Tool for WebSearchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::network_read(ToolSpec {
            name: "web_search".into(),
            description: "Search the public web through Firecrawl. Use it for current, \
                          unstable, or externally sourced information."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "A concise web search query."
                    }
                },
                "required": ["query"]
            }),
        })
    }

    async fn call(&self, args: Value, cancel: &CancellationToken) -> Result<ToolOutcome> {
        if cancel.is_cancelled() {
            return Ok(cancelled_outcome());
        }
        let query = args["query"].as_str().unwrap_or_default().trim();
        if query.is_empty() {
            return Ok(failed_outcome("missing `query`"));
        }
        let config = self.config.read().await.clone();
        if !config.web_search_enabled {
            return Ok(failed_outcome(
                "web search is disabled; the user can enable it with /websearch",
            ));
        }
        let bundle = firecrawl_search(&config, query).await;
        let ok = !bundle.text.starts_with("[Firecrawl");
        Ok(ToolOutcome {
            content: cap(&bundle.text, MAX_TOOL_OUTPUT),
            ok,
            touched: vec![],
            patches: vec![],
        })
    }
}

pub struct WebFetchTool {
    pub config: Arc<RwLock<AppConfig>>,
}

#[async_trait::async_trait]
impl Tool for WebFetchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::network_read(ToolSpec {
            name: "web_fetch".into(),
            description: "Fetch and extract one public webpage through Firecrawl.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "An http:// or https:// URL."
                    }
                },
                "required": ["url"]
            }),
        })
    }

    async fn call(&self, args: Value, cancel: &CancellationToken) -> Result<ToolOutcome> {
        if cancel.is_cancelled() {
            return Ok(cancelled_outcome());
        }
        let url = args["url"].as_str().unwrap_or_default().trim();
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Ok(failed_outcome("`url` must start with http:// or https://"));
        }
        let config = self.config.read().await.clone();
        if !config.web_search_enabled {
            return Ok(failed_outcome(
                "web search is disabled; the user can enable it with /websearch",
            ));
        }
        let entry = firecrawl_fetch(&config, url).await;
        let content = format!(
            "Title: {}\nURL: {}\nDescription: {}\n\n{}",
            entry.title, entry.url, entry.description, entry.content
        );
        let ok = !entry.content.starts_with("[Firecrawl");
        Ok(ToolOutcome {
            content: cap(&content, MAX_TOOL_OUTPUT),
            ok,
            touched: vec![],
            patches: vec![],
        })
    }
}

fn failed_outcome(message: &str) -> ToolOutcome {
    ToolOutcome {
        content: message.to_string(),
        ok: false,
        touched: vec![],
        patches: vec![],
    }
}

fn cancelled_outcome() -> ToolOutcome {
    failed_outcome("tool call cancelled")
}

// ---------------------------------------------------------------------------
// sudo / stored output
// ---------------------------------------------------------------------------

pub struct SudoTool {
    pub policy: SandboxPolicy,
    pub broker: Arc<PrivilegeBroker>,
}

#[async_trait::async_trait]
impl Tool for SudoTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::privileged(ToolSpec {
            name: "sudo".into(),
            description: "Run one command as root after an explicit approval and local masked authentication. The password is handled only by the interface and is never visible to the model. Pass the command without a sudo prefix. Disabled in read-only mode.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Command to run as root, without a sudo prefix."
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "description": "Optional. Defaults to 120000 and is capped at 600000."
                    }
                },
                "required": ["command"]
            }),
        })
    }

    async fn call(&self, args: Value, cancel: &CancellationToken) -> Result<ToolOutcome> {
        if matches!(
            self.policy.mode,
            SandboxMode::ReadOnly | SandboxMode::IsolatedWorkspaceWrite
        ) {
            return Ok(failed_outcome(
                "sudo is disabled by the active isolated/read-only execution mode",
            ));
        }
        let command = args["command"].as_str().unwrap_or_default().trim();
        if command.is_empty() {
            return Ok(failed_outcome("missing `command`"));
        }
        if command.contains('\0') {
            return Ok(failed_outcome("command contains a NUL byte"));
        }
        if command_requests_privilege(command) {
            return Ok(failed_outcome(
                "pass the root command without a nested sudo prefix",
            ));
        }

        self.broker.ensure_authenticated(command, cancel).await?;
        let mut policy = self.policy.clone();
        policy.allow_privilege_escalation = true;
        if let Some(timeout_ms) = args["timeout_ms"].as_u64() {
            policy.timeout_ms = timeout_ms.clamp(1_000, 600_000);
        }
        let output = spawn_sandboxed_with_cancel(
            &policy,
            "sudo",
            &[
                "-n".into(),
                "--".into(),
                "/bin/sh".into(),
                "-c".into(),
                command.into(),
            ],
            cancel,
        )
        .await?;

        let mut body = String::new();
        if !output.stdout.is_empty() {
            body.push_str(&output.stdout);
        }
        if !output.stderr.is_empty() {
            body.push_str("\n--- stderr ---\n");
            body.push_str(&output.stderr);
        }
        if output.timed_out {
            body.push_str("\n[command timed out]");
        }
        if output.cancelled {
            body.push_str("\n[command interrupted]");
        }
        if body.trim().is_empty() {
            body = "(no output)".into();
        }
        let code = output.exit_code.unwrap_or(-1);
        Ok(ToolOutcome {
            content: format!("exit {code}\n\n{body}"),
            ok: code == 0 && !output.timed_out && !output.cancelled,
            touched: vec![policy.cwd],
            patches: vec![],
        })
    }
}

pub struct ReadToolOutputTool {
    pub store: Arc<ToolOutputStore>,
}

#[async_trait::async_trait]
impl Tool for ReadToolOutputTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::workspace_read(ToolSpec {
            name: "read_tool_output".into(),
            description: "Read a line range from a complete tool result that was stored outside the conversation. Use the opaque handle shown in the truncated preview.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "handle": { "type": "string" },
                    "offset": { "type": "integer", "description": "First line, 1-based." },
                    "limit": { "type": "integer", "description": "Maximum lines, from 1 to 800." }
                },
                "required": ["handle"]
            }),
        })
    }

    async fn call(&self, args: Value, _cancel: &CancellationToken) -> Result<ToolOutcome> {
        let handle = args["handle"].as_str().unwrap_or_default();
        let offset = args["offset"].as_u64().unwrap_or(1) as usize;
        let limit = args["limit"].as_u64().unwrap_or(400) as usize;
        match self.store.read(handle, offset, limit) {
            Ok(content) => Ok(ToolOutcome {
                content,
                ok: true,
                touched: vec![],
                patches: vec![],
            }),
            Err(error) => Ok(failed_outcome(&error.to_string())),
        }
    }
}

// ---------------------------------------------------------------------------
// Agent Skills
// ---------------------------------------------------------------------------

pub struct ActivateSkillTool {
    pub workspace: PathBuf,
}

#[async_trait::async_trait]
impl Tool for ActivateSkillTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::workspace_read(ToolSpec {
            name: "activate_skill".into(),
            description: "Load the complete SKILL.md instructions for one installed skill. \
                          Use this only when the startup skill catalog clearly matches the \
                          user's current task. A skill never grants permissions."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Exact skill name from the installed catalog"
                    }
                },
                "required": ["name"]
            }),
        })
    }

    async fn call(&self, args: Value, _cancel: &CancellationToken) -> Result<ToolOutcome> {
        let Some(name) = args["name"].as_str() else {
            return Ok(failed_outcome("missing `name`"));
        };
        match skills::load(&self.workspace, name) {
            Ok(skill) => Ok(ToolOutcome {
                content: skills::render_for_model(&skill),
                ok: true,
                touched: Vec::new(),
                patches: Vec::new(),
            }),
            Err(error) => Ok(failed_outcome(&error.to_string())),
        }
    }
}

pub struct ReadSkillResourceTool {
    pub workspace: PathBuf,
}

#[async_trait::async_trait]
impl Tool for ReadSkillResourceTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::workspace_read(ToolSpec {
            name: "read_skill_resource".into(),
            description: "Read one text resource inside an installed skill after activating \
                          that skill. Traversal, symlinks, binary files and oversized resources \
                          are rejected."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "skill": { "type": "string" },
                    "path": {
                        "type": "string",
                        "description": "Path relative to the skill root, e.g. references/api.md"
                    }
                },
                "required": ["skill", "path"]
            }),
        })
    }

    async fn call(&self, args: Value, _cancel: &CancellationToken) -> Result<ToolOutcome> {
        let Some(skill) = args["skill"].as_str() else {
            return Ok(failed_outcome("missing `skill`"));
        };
        let Some(path) = args["path"].as_str() else {
            return Ok(failed_outcome("missing `path`"));
        };
        match skills::read_resource(&self.workspace, skill, path) {
            Ok(content) => Ok(ToolOutcome {
                content: cap(&content, MAX_TOOL_OUTPUT),
                ok: true,
                touched: Vec::new(),
                patches: Vec::new(),
            }),
            Err(error) => Ok(failed_outcome(&error.to_string())),
        }
    }
}

pub struct LearnSkillTool;

#[async_trait::async_trait]
impl Tool for LearnSkillTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::user_process(ToolSpec {
            name: "learn_skill".into(),
            description: "Create or update a persistent SKILL.md from a reusable workflow, but only when the user explicitly asks GnomeAI to learn or remember it. An optional script must be POSIX shell. This call stores the skill and never executes it.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Lowercase letters, digits and single hyphens"},
                    "description": {"type": "string"},
                    "instructions": {"type": "string"},
                    "script": {"type": "string", "description": "Optional POSIX shell entrypoint"},
                    "platforms": {"type": "array", "items": {"type": "string"}},
                    "replace": {"type": "boolean", "default": false}
                },
                "required": ["name", "description", "instructions"],
                "additionalProperties": false
            }),
        })
    }

    async fn call(&self, args: Value, _cancel: &CancellationToken) -> Result<ToolOutcome> {
        let platforms = args["platforms"]
            .as_array()
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let spec = skills::LearnedSkillSpec {
            name: args["name"].as_str().unwrap_or_default().trim().into(),
            description: args["description"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .into(),
            instructions: args["instructions"]
                .as_str()
                .unwrap_or_default()
                .trim()
                .into(),
            script: args["script"].as_str().map(str::to_string),
            platforms,
            replace: args["replace"].as_bool().unwrap_or(false),
        };
        match tokio::task::spawn_blocking(move || skills::learn(spec)).await {
            Ok(Ok(skill)) => Ok(ToolOutcome {
                content: serde_json::to_string_pretty(&skill)?,
                ok: true,
                touched: Vec::new(),
                patches: Vec::new(),
            }),
            Ok(Err(error)) => Ok(failed_outcome(&error.to_string())),
            Err(error) => Ok(failed_outcome(&error.to_string())),
        }
    }
}

pub struct RunSkillTool {
    pub workspace: PathBuf,
    pub policy: SandboxPolicy,
    pub config: Arc<RwLock<AppConfig>>,
}

#[async_trait::async_trait]
impl Tool for RunSkillTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::user_process(ToolSpec {
            name: "run_skill".into(),
            description: "Run the declared POSIX-shell entrypoint of an installed skill, locally or on an exact paired node. This is separate from activation and always follows process approval. Remote root additionally follows the selected device policy.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "target": {"type": "string", "enum": ["local", "node"], "default": "local"},
                    "node_id": {"type": "string"},
                    "cwd": {"type": "string"},
                    "timeout_secs": {"type": "integer", "minimum": 1, "maximum": 3600},
                    "root": {"type": "boolean", "default": false}
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        })
    }

    async fn call(&self, args: Value, cancel: &CancellationToken) -> Result<ToolOutcome> {
        let name = args["name"].as_str().unwrap_or_default().trim();
        let entrypoint = match skills::entrypoint(&self.workspace, name) {
            Ok(entrypoint) => entrypoint,
            Err(error) => return Ok(failed_outcome(&error.to_string())),
        };
        let timeout_secs = args["timeout_secs"].as_u64().unwrap_or(120).clamp(1, 3_600);
        match args["target"].as_str().unwrap_or("local") {
            "local" => {
                if args["root"].as_bool().unwrap_or(false) {
                    return Ok(failed_outcome(
                        "local root is not implied by run_skill; use the dedicated sudo tool",
                    ));
                }
                let mut policy = self.policy.clone();
                policy.timeout_ms = timeout_secs * 1_000;
                if let Some(cwd) = args["cwd"].as_str() {
                    policy.cwd = PathBuf::from(cwd);
                }
                let output = spawn_sandboxed_with_cancel(
                    &policy,
                    "/bin/sh",
                    &[entrypoint.path.to_string_lossy().into_owned()],
                    cancel,
                )
                .await?;
                let mut body = output.stdout;
                if !output.stderr.is_empty() {
                    body.push_str("\n--- stderr ---\n");
                    body.push_str(&output.stderr);
                }
                let code = output.exit_code.unwrap_or(-1);
                Ok(ToolOutcome {
                    content: format!("{}: exit {code}\n\n{body}", entrypoint.relative),
                    ok: code == 0 && !output.timed_out && !output.cancelled,
                    touched: vec![policy.cwd],
                    patches: Vec::new(),
                })
            }
            "node" => {
                let cfg = self.config.read().await.clone();
                if !cfg.node_hub_enabled {
                    return Ok(failed_outcome("remote nodes are disabled"));
                }
                let node_id = args["node_id"].as_str().unwrap_or_default().trim();
                if node_id.is_empty() {
                    return Ok(failed_outcome("node_id is required for target=node"));
                }
                let client = nodes::local_client(cfg.node_hub_port, &cfg.node_hub_admin_token);
                match client
                    .execute(
                        node_id,
                        &QueueJobRequest {
                            action: "script".into(),
                            command: String::new(),
                            stdin: entrypoint.script,
                            cwd: args["cwd"].as_str().map(str::to_string),
                            timeout_secs,
                            root: args["root"].as_bool().unwrap_or(false),
                            root_approved: self.policy.mode == SandboxMode::Normal,
                        },
                    )
                    .await
                {
                    Ok(value) => Ok(ToolOutcome {
                        content: serde_json::to_string_pretty(&value)?,
                        ok: value.get("ok").and_then(Value::as_bool).unwrap_or(true),
                        touched: Vec::new(),
                        patches: Vec::new(),
                    }),
                    Err(error) => Ok(failed_outcome(&error.to_string())),
                }
            }
            other => Ok(failed_outcome(&format!("unknown skill target `{other}`"))),
        }
    }
}

// ---------------------------------------------------------------------------
// graphical desktop
// ---------------------------------------------------------------------------

pub struct DesktopTool {
    pub generated_dir: PathBuf,
    pub enabled: bool,
}

#[async_trait::async_trait]
impl Tool for DesktopTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::user_process(ToolSpec {
            name: "desktop".into(),
            description: "Navigate the graphical Linux desktop. Prefer semantic AT-SPI actions: inspect returns controls with opaque target IDs for the current UI, then activate/set_text/focus acts without pixel guessing and returns an updated semantic tree. Use observe and coordinate actions only for inaccessible/canvas UI. Desktop control is disabled in read-only mode and follows normal/full-access approval policy.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["inspect", "activate", "set_text", "focus", "observe", "click", "double_click", "move", "type", "key", "scroll", "focus_window"]
                    },
                    "query": {"type": "string", "description": "Optional semantic name/role/action filter"},
                    "limit": {"type": "integer", "default": 140, "minimum": 1, "maximum": 400},
                    "target": {"type": "string", "description": "Opaque a11y: target returned by inspect"},
                    "action_name": {"type": "string", "description": "Optional accessible action; the default action is used when omitted"},
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
        })
    }

    async fn call(&self, args: Value, cancel: &CancellationToken) -> Result<ToolOutcome> {
        if !self.enabled {
            return Ok(failed_outcome(
                "desktop automation is disabled in read-only mode",
            ));
        }
        let Some(args) = args.as_object() else {
            return Ok(failed_outcome("desktop arguments must be an object"));
        };
        match desktop::perform(&self.generated_dir, args, cancel).await {
            Ok(result) => Ok(ToolOutcome {
                content: serde_json::to_string_pretty(&result)?,
                ok: true,
                touched: Vec::new(),
                patches: Vec::new(),
            }),
            Err(error) => Ok(failed_outcome(&error.to_string())),
        }
    }
}

// ---------------------------------------------------------------------------
// remote execution nodes
// ---------------------------------------------------------------------------

pub struct NodeTool {
    pub config: Arc<RwLock<AppConfig>>,
    pub mode: SandboxMode,
}

#[async_trait::async_trait]
impl Tool for NodeTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::user_process(ToolSpec {
            name: "node".into(),
            description: "List paired lightweight GnomeAI devices or execute one shell command on a selected device. Nodes connect outbound and may use runit, OpenRC, s6, systemd, another supervisor, or no service manager. List first and use the exact node_id. Remote root also requires the per-device policy configured in the main app.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["list", "exec"]},
                    "node_id": {"type": "string"},
                    "command": {"type": "string"},
                    "cwd": {"type": "string"},
                    "timeout_secs": {"type": "integer", "minimum": 1, "maximum": 3600},
                    "root": {"type": "boolean", "default": false}
                },
                "required": ["action"],
                "additionalProperties": false
            }),
        })
    }

    async fn call(&self, args: Value, _cancel: &CancellationToken) -> Result<ToolOutcome> {
        let cfg = self.config.read().await.clone();
        if !cfg.node_hub_enabled {
            return Ok(failed_outcome(
                "remote nodes are disabled; enable the Hub in Settings and restart GnomeAI",
            ));
        }
        let client = nodes::local_client(cfg.node_hub_port, &cfg.node_hub_admin_token);
        let action = args["action"].as_str().unwrap_or("list");
        let result = match action {
            "list" => client.list().await,
            "exec" => {
                let node_id = args["node_id"].as_str().unwrap_or_default().trim();
                let command = args["command"].as_str().unwrap_or_default().trim();
                if node_id.is_empty() || command.is_empty() {
                    return Ok(failed_outcome("node_id and command are required for exec"));
                }
                client
                    .execute(
                        node_id,
                        &QueueJobRequest {
                            action: "shell".into(),
                            command: command.into(),
                            stdin: String::new(),
                            cwd: args["cwd"].as_str().map(str::to_string),
                            timeout_secs: args["timeout_secs"]
                                .as_u64()
                                .unwrap_or(60)
                                .clamp(1, 3_600),
                            root: args["root"].as_bool().unwrap_or(false),
                            // In normal mode this call only starts after the
                            // user approved this particular remote command.
                            // Full access remains user-level and never implies
                            // a one-shot root approval.
                            root_approved: self.mode == SandboxMode::Normal,
                        },
                    )
                    .await
            }
            other => return Ok(failed_outcome(&format!("unknown node action `{other}`"))),
        };
        match result {
            Ok(value) => Ok(ToolOutcome {
                content: serde_json::to_string_pretty(&value)?,
                ok: value.get("ok").and_then(Value::as_bool).unwrap_or(true),
                touched: Vec::new(),
                patches: Vec::new(),
            }),
            Err(error) => Ok(failed_outcome(&error.to_string())),
        }
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

pub fn register_all(
    registry: &mut Registry,
    root: &Path,
    generated_dir: &Path,
    policy: SandboxPolicy,
    config: Arc<RwLock<AppConfig>>,
    output_store: Arc<ToolOutputStore>,
    privilege_broker: Arc<PrivilegeBroker>,
) {
    registry.register(Arc::new(DesktopTool {
        generated_dir: generated_dir.to_path_buf(),
        enabled: policy.mode != SandboxMode::ReadOnly,
    }));
    registry.register(Arc::new(NodeTool {
        config: config.clone(),
        mode: policy.mode,
    }));
    registry.register(Arc::new(LearnSkillTool));
    registry.register(Arc::new(RunSkillTool {
        workspace: root.to_path_buf(),
        policy: policy.clone(),
        config: config.clone(),
    }));
    registry.register(Arc::new(ListFilesTool {
        root: root.to_path_buf(),
    }));
    registry.register(Arc::new(ReadFileTool {
        root: root.to_path_buf(),
    }));
    registry.register(Arc::new(SearchTool {
        root: root.to_path_buf(),
    }));
    registry.register(Arc::new(ApplyPatchTool {
        root: root.to_path_buf(),
        writable: policy.mode != SandboxMode::ReadOnly,
    }));
    registry.register(Arc::new(ShellTool {
        policy: policy.clone(),
    }));
    registry.register(Arc::new(SudoTool {
        policy,
        broker: privilege_broker,
    }));
    registry.register(Arc::new(ReadToolOutputTool {
        store: output_store,
    }));
    registry.register(Arc::new(ActivateSkillTool {
        workspace: root.to_path_buf(),
    }));
    registry.register(Arc::new(ReadSkillResourceTool {
        workspace: root.to_path_buf(),
    }));
    registry.register(Arc::new(WebSearchTool {
        config: config.clone(),
    }));
    registry.register(Arc::new(WebFetchTool { config }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_keeps_both_ends() {
        let s = format!("HEAD{}TAIL", "x".repeat(10_000));
        let c = cap(&s, 100);
        assert!(c.starts_with("HEAD"));
        assert!(c.ends_with("TAIL"));
    }

    #[test]
    fn glob_basics() {
        assert!(glob_match("*.rs", "main.rs"));
        assert!(!glob_match("*.rs", "main.py"));
        assert!(glob_match("Cargo.*", "Cargo.toml"));
    }
}
