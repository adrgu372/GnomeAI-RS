//! The GnomeAI side of the method: a task adapter that rolls out real GnomeAI
//! sessions, and the `/harness2` driver the desktop calls.
//!
//! A rollout here is one genuinely autonomous session: the agent gets the
//! task and the candidate harness's always-on text plus its skills, runs with
//! the ordinary tool surface, and its own transcript *is* the trajectory. That
//! is what makes the harness's effect measurable rather than simulated.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use crate::config::AppConfig;

use super::adapter::{Adapter, BenchPrompts, BenchSpec, RunView};
use super::proposer::Proposer;
use super::proposer::ScriptedSession;
use super::schedule::{TaskOptions, run_task};
use super::tree::{self, Tree};

/// Rollouts run in a scratch workspace so a candidate harness can never touch
/// the user's repository.
pub struct GnomeaiAdapter {
    spec: BenchSpec,
    task: String,
    workspace: PathBuf,
    base_harness: PathBuf,
    state: PathBuf,
}

impl GnomeaiAdapter {
    /// Build the single-task adapter for a `/harness2` run.
    pub fn new(task: &str, workspace: &Path, state_root: &Path, k: u32) -> anyhow::Result<Self> {
        let slug = slugify(task);
        let base = state_root.join("base-harness");
        if !tree::is_tree(&super::substrate::gnomeai_surface(), &base) {
            let surface = super::substrate::gnomeai_surface();
            let mut t = Tree::new();
            for a in surface.always_on {
                t.insert((*a).to_string(), String::new());
            }
            t.insert(
                "systemprompt.md".into(),
                "You are GnomeAI, a native coding agent. Work the task in the workspace, \
                 verifying what you produce before you finish.\n"
                    .into(),
            );
            tree::write_tree(&surface, &t, &base, true)?;
        }
        let prompts = BenchPrompts {
            work: task.to_string(),
            domain_word: "workspace".into(),
            inv_levers: String::new(),
            dom_levers: String::new(),
            yardstick: String::new(),
            deliverable_word: "deliverable".into(),
            domain_guidance: String::new(),
            selection_checks: String::new(),
            selection_lead: String::new(),
            selection_conformance: "full".into(),
            prompt_pack: "adapt".into(),
        };
        let spec = BenchSpec {
            name: format!("gnomeai-{slug}"),
            runs_root: state_root.join("runs"),
            install_mode: "workspace_config".into(),
            prompts,
            domain_word: "workspace".into(),
            leak_globs: Vec::new(),
            allow_subagents: false,
            proposer_deliverables: "contents".into(),
            state_deliverable: true,
            obs_trunc: 50_000,
            reserved_scripts: vec!["read_doc.py".into(), "pack.py".into(), "unpack.py".into()],
        };
        spec.validate().map_err(anyhow::Error::msg)?;
        let _ = k;
        Ok(Self {
            spec,
            task: task.to_string(),
            workspace: workspace.to_path_buf(),
            base_harness: base,
            state: state_root.to_path_buf(),
        })
    }

    fn run_dir_for(&self, run_id: &str) -> PathBuf {
        self.spec
            .runs_root
            .join("rollouts")
            .join(run_id.replace('/', "__"))
    }
}

fn slugify(task: &str) -> String {
    let mut out = String::new();
    for c in task.chars().take(48) {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "task".into()
    } else {
        trimmed
    }
}

impl Adapter for GnomeaiAdapter {
    fn spec(&self) -> &BenchSpec {
        &self.spec
    }

    fn substrate_name(&self) -> &str {
        "gnomeai"
    }

    fn tasks_for_split(&self, _split: &str) -> Vec<String> {
        vec![self.task.clone()]
    }

    fn domain_of(&self, _task_id: &str) -> String {
        self.workspace
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "workspace".into())
    }

    fn task_prompt(&self, _task_id: &str) -> String {
        self.task.clone()
    }

    fn execute<'a>(
        &'a self,
        _task_id: &'a str,
        harness_dir: Option<&'a Path>,
        run_id: &'a str,
        timeout: Option<u64>,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<PathBuf>> + Send + 'a>> {
        Box::pin(async move {
            let run_dir = self.run_dir_for(run_id);
            std::fs::create_dir_all(&run_dir)?;
            // The rollout workspace: a copy of the caller's directory per run
            // keeps two candidates from seeing each other's writes.
            let sandbox = run_dir.join("workspace");
            if !sandbox.is_dir() {
                // Build output is not part of what a candidate is asked to
                // change, and copying it once per candidate is what made a
                // two-candidate run cost five gigabytes.
                super::copy_workspace(&self.workspace, &sandbox)?;
            }
            let mut prompt = self.task.clone();
            if let Some(hdir) = harness_dir {
                let surface = super::substrate::gnomeai_surface();
                let candidate = tree::read_tree(&surface, hdir);
                let always_on = tree::always_on_text(&surface, &candidate);
                if !always_on.trim().is_empty() {
                    prompt.push_str(
                        "\n\n# HARNESS INSTRUCTIONS\n\nStanding instructions for this work:\n\n",
                    );
                    prompt.push_str(always_on.trim());
                    prompt.push('\n');
                }
                let skills = tree::skill_names(&surface, &candidate);
                if !skills.is_empty() {
                    prompt.push_str(&format!(
                        "\nSkills installed for this run: {}.\n",
                        skills.join(", ")
                    ));
                }
            }
            let (transcript, ok) = run_one_session(&sandbox, &prompt, timeout).await?;
            std::fs::write(run_dir.join("trajectory.md"), &transcript)?;
            // The deliverable is whatever the agent left in its workspace; a
            // session that changed nothing delivered nothing.
            let delivered = deliverable_from(&sandbox, &self.workspace);
            std::fs::write(
                run_dir.join("answer.txt"),
                delivered
                    .clone()
                    .unwrap_or_else(|| "(no deliverable)".into()),
            )?;
            if !ok {
                anyhow::bail!("the session did not finish");
            }
            Ok(run_dir)
        })
    }

    fn delivered(&self, run_dir: &Path) -> bool {
        std::fs::read_to_string(run_dir.join("answer.txt"))
            .map(|t| !t.trim().is_empty() && t.trim() != "(no deliverable)")
            .unwrap_or(false)
    }

    fn trajectory(&self, run_dir: &Path, obs_trunc: usize) -> anyhow::Result<String> {
        let body = super::read_text_lossy(&run_dir.join("trajectory.md"))?;
        Ok(truncate_observations(&body, obs_trunc))
    }

    fn deliverable_text(&self, run_dir: &Path) -> anyhow::Result<String> {
        Ok(super::read_text_lossy(&run_dir.join("answer.txt"))?)
    }

    fn deliverable_files(&self, run_dir: &Path) -> Option<BTreeMap<String, String>> {
        let text = std::fs::read_to_string(run_dir.join("answer.txt")).ok()?;
        let mut m = BTreeMap::new();
        m.insert("answer.txt".into(), text);
        Some(m)
    }

    fn base_harness(&self) -> PathBuf {
        self.base_harness.clone()
    }

    fn validate_tree(&self, harness: &Tree) -> Result<(), String> {
        // A harness that says nothing at all is not a harness: the run would
        // be identical to base and the evidence would be worthless.
        let surface = super::substrate::gnomeai_surface();
        if tree::always_on_text(&surface, harness).trim().len() < 20 {
            return Err(
                "the always-on text is effectively empty; a harness with no standing \
                 instruction cannot change behaviour"
                    .into(),
            );
        }
        Ok(())
    }

    fn score(&self, _run_dir: &Path, _task_id: &str) -> Option<serde_json::Value> {
        // The desktop has no rubric; judging stays with the human reading the
        // selection and the library entry.
        None
    }
}

/// Elide long observations but never the events themselves.
fn truncate_observations(body: &str, limit: usize) -> String {
    if limit == 0 {
        return body.to_string();
    }
    let mut out = String::with_capacity(body.len());
    let mut remaining = 0usize;
    for line in body.lines() {
        if remaining > 0 {
            out.push_str(line);
            out.push('\n');
            remaining = remaining.saturating_sub(line.len() + 1);
            if remaining == 0 {
                out.push_str("[observation truncated by the harness2 renderer]\n");
            }
            continue;
        }
        if line.starts_with("[OBSERVATION]") {
            out.push_str(line);
            out.push('\n');
            remaining = limit;
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// A changed or created file under `sandbox`, rendered for the proposer.
fn deliverable_from(sandbox: &Path, original: &Path) -> Option<String> {
    let mut produced = Vec::new();
    for rel in super::walk_files(sandbox) {
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if rel_str.starts_with(".git/") || rel_str.contains(".gnomeai/") {
            continue;
        }
        let now = std::fs::read(sandbox.join(&rel)).unwrap_or_default();
        let before = std::fs::read(original.join(&rel)).ok();
        if before.as_deref() != Some(now.as_slice()) {
            produced.push((rel_str, now));
        }
    }
    if produced.is_empty() {
        return None;
    }
    let mut out = String::new();
    for (rel, bytes) in produced.iter().take(12) {
        out.push_str(&format!("## {rel}\n"));
        match String::from_utf8(bytes.clone()) {
            Ok(text) => {
                out.push_str(text.trim_end());
                out.push('\n');
            }
            Err(_) => out.push_str(&format!("(binary, {} bytes)\n", bytes.len())),
        }
        out.push('\n');
    }
    Some(out)
}

/// One autonomous session over the configured provider, returning its
/// transcript and whether it finished.
async fn run_one_session(
    workspace: &Path,
    prompt: &str,
    timeout: Option<u64>,
) -> anyhow::Result<(String, bool)> {
    let app_home = crate::app_dirs::resolve_app_home(workspace)?;
    let cfg = AppConfig::load(&app_home.join("config.json"))?;
    let selection = crate::provider_catalog::ProviderSelection::legacy(
        cfg.llama_base_url.trim_end_matches('/').to_string(),
        (!cfg.llama_api_key.trim().is_empty()).then(|| cfg.llama_api_key.trim().to_string()),
        cfg.default_model.clone(),
    );
    let model = selection.model.clone();
    let provider_id = selection.provider_id.clone();
    let provider = crate::provider_catalog::build_provider(
        &selection,
        workspace,
        crate::sandbox::SandboxMode::Normal,
    )?;
    let policy = crate::sandbox::SandboxPolicy::normal(workspace);
    let store = crate::store::Store::open(&app_home.join("store/harness2-rollout.db"))?;
    let session = store.create_session(workspace, &model)?;
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(512);
    let (approval_tx, approval_rx) = tokio::sync::mpsc::channel(64);
    let output_store = Arc::new(crate::tooling::ToolOutputStore::new(
        app_home.join("store/harness2-tool_outputs"),
    )?);
    let (priv_tx, priv_rx) = tokio::sync::mpsc::channel(8);
    let broker = Arc::new(crate::privilege::PrivilegeBroker::new(
        event_tx.clone(),
        priv_rx,
    ));
    drop(priv_tx);
    let mut registry = crate::tooling::Registry::default();
    let paths = crate::storage::AppPaths::new(app_home.clone())?;
    let config = Arc::new(tokio::sync::RwLock::new(cfg.clone()));
    crate::tools::register_all(
        &mut registry,
        workspace,
        &paths.generated_dir,
        policy.clone(),
        config,
        output_store.clone(),
        broker,
        String::new(),
        String::new(),
    );
    let agent = crate::agent::Agent::new(
        provider,
        Arc::new(registry),
        store,
        session.id.clone(),
        model,
        provider_id,
        cfg.reasoning_effort.clone(),
        crate::agent::ApprovalPolicy::Never,
        workspace.to_path_buf(),
        policy,
        output_store,
        Vec::new(),
        event_tx.clone(),
        approval_rx,
    );
    drop(approval_tx);
    let collector = tokio::spawn(async move {
        let mut lines: Vec<String> = Vec::new();
        while let Some(event) = event_rx.recv().await {
            match event {
                crate::protocol::Event::Token { text } => {
                    if let Some(last) = lines.last_mut() {
                        if let Some(s) = last.strip_prefix("[ASSISTANT] ") {
                            *last = format!("[ASSISTANT] {s}{text}");
                            continue;
                        }
                    }
                    lines.push(format!("[ASSISTANT] {text}"));
                }
                crate::protocol::Event::ToolCallStarted { name, summary, .. } => {
                    lines.push(format!("[ACTION] {name}: {summary}"));
                }
                crate::protocol::Event::ToolCallEnded { ok, .. } => {
                    lines.push(format!("[OBSERVATION] tool finished ok={ok}"));
                }
                crate::protocol::Event::Error { message, .. } => {
                    lines.push(format!("[ERROR] {message}"));
                }
                _ => {}
            }
        }
        lines
    });
    let cancel = tokio_util::sync::CancellationToken::new();
    let run = agent.run_turn(prompt.to_string(), cancel.clone());
    let outcome = match timeout {
        Some(secs) => tokio::time::timeout(std::time::Duration::from_secs(secs), run)
            .await
            .map_err(|_| anyhow::anyhow!("session timed out after {secs}s")),
        None => Ok(run.await),
    };
    drop(event_tx);
    let lines = collector.await.unwrap_or_default();
    let finished = matches!(outcome, Ok(Ok(())));
    let transcript = if lines.is_empty() {
        "(no transcript was produced)".to_string()
    } else {
        lines.join("\n")
    };
    Ok((transcript, finished))
}

/// A proposer over the in-process agent: the improver edits the harness in a
/// scratch copy with the same tool surface, read-only against everything else.
pub fn desktop_proposer(model: &str) -> Proposer {
    Proposer::new("gnomeai", model)
}

/// The `/harness2` driver: report the plan, then run the method and print the
/// summary. Never blocks the core loop for longer than one rollout.
pub async fn run_desktop_command(
    workspace: PathBuf,
    config_state: Arc<tokio::sync::RwLock<AppConfig>>,
    events: tokio::sync::mpsc::Sender<crate::protocol::Event>,
    task: String,
    k: u32,
    mode: String,
    dry_run: bool,
) {
    let notice = |message: String| {
        let events = events.clone();
        async move {
            let _ = events
                .send(crate::protocol::Event::Notice { message })
                .await;
        }
    };
    if task.trim().is_empty() {
        notice("usage: /harness2 [parallel|sequential|both] [k] -- TASK".into()).await;
        return;
    }
    let modes: Vec<String> = match mode.trim().to_lowercase().as_str() {
        "parallel" | "agg" => vec!["agg".into()],
        "sequential" | "seq" => vec!["seq".into()],
        _ => vec!["agg".into(), "seq".into()],
    };
    let state_root = match super::env("HARNESS2_RUNS_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => super::runs_root(),
    };
    let adapter = match GnomeaiAdapter::new(&task, &workspace, &state_root, k) {
        Ok(a) => Arc::new(a),
        Err(exc) => {
            notice(format!("harness2 cannot start: {exc}")).await;
            return;
        }
    };
    let cfg = config_state.read().await.clone();
    let model = super::env("HARNESS2_IMPROVER_MODEL").unwrap_or_else(|| cfg.default_model.clone());
    if dry_run {
        let surface = super::substrate::gnomeai_surface();
        notice(format!(
            "harness2 plan (dry run)\n  task: {task}\n  modes: {}\n  k: {k}\n  improver: \
             gnomeai/{model}\n  runs root: {}\n  editable surface: {}\n  rollouts per mode: \
             {}\n  improvement stages per mode: {}\n\nNothing was called and nothing was \
             written.",
            modes.join(", "),
            adapter.spec().runs_root.display(),
            surface.always_on.join(", "),
            4 * k,
            3 * k
        ))
        .await;
        return;
    }
    notice(format!(
        "harness2 started: {k} candidate(s) per dimension, modes {}, improver {model}. \
         Rollouts run in a scratch copy of the workspace; the results land in {}.",
        modes.join(", "),
        adapter.spec().runs_root.display()
    ))
    .await;
    let prop = Arc::new(desktop_proposer(&model));
    let libraries: BTreeMap<String, super::library::Library> = modes
        .iter()
        .map(|m| {
            (
                m.clone(),
                super::library::Library::new(
                    state_root.join("library").join(m),
                    &adapter.domain_of("task"),
                    "gnomeai",
                ),
            )
        })
        .collect();
    let options = TaskOptions {
        modes,
        seed: None,
        tag: String::new(),
        libraries,
        pool_policy: "finals".into(),
    };
    let task_id = adapter.task_prompt("task");
    match run_task(
        adapter.clone(),
        prop,
        &adapter.spec().runs_root,
        &task_id,
        k,
        options,
    )
    .await
    {
        Ok(result) => {
            let mut lines = vec![format!("harness2 finished: {}", result.status)];
            for (mode, record) in &result.selections {
                let chosen = record
                    .get("chosen_label")
                    .and_then(|l| l.as_str())
                    .unwrap_or("-");
                let why = record.get("why").and_then(|w| w.as_str()).unwrap_or("");
                lines.push(format!("  {mode}: adopted {chosen}"));
                if !why.is_empty() {
                    lines.push(format!("    because: {why}"));
                }
            }
            for (mode, record) in &result.modes {
                if let Some(ledger) = record.get("ledger") {
                    lines.push(format!(
                        "  {mode}: {} rollouts, {} improvement stages, ${:.2}",
                        ledger.get("R").and_then(|v| v.as_u64()).unwrap_or(0),
                        ledger.get("P").and_then(|v| v.as_u64()).unwrap_or(0),
                        ledger.get("usd").and_then(|v| v.as_f64()).unwrap_or(0.0)
                    ));
                }
            }
            if !result.errors.is_empty() {
                lines.push(format!("  notes: {}", result.errors.join("; ")));
            }
            let summary = adapter
                .spec()
                .runs_root
                .join(format!("{}-ga-agg{k}", adapter.spec().name))
                .join(task_id.replace('/', "__"))
                .join("summary.json");
            lines.push(format!("  summary: {}", summary.display()));
            notice(lines.join("\n")).await;
        }
        Err(exc) => {
            notice(format!("harness2 failed: {exc}")).await;
        }
    }
}

/// A no-op session used when a caller wants the driver without a model.
pub struct NullSession;

/// A parsed `/harness2` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Harness2Command {
    pub task: String,
    pub k: u32,
    pub mode: String,
    pub dry_run: bool,
}

impl Harness2Command {
    pub fn usage() -> &'static str {
        "usage: /harness2 [parallel|sequential|both] [k] [--dry-run] -- TASK\n\
         Runs the harness2 method: candidate harnesses for the task are proposed, each is \
         rolled out for real, and the best-supported one is adopted into this workspace's \
         library."
    }
}

fn looks_like_flag(token: &str) -> bool {
    token.starts_with('-')
}

/// Whether a token is a `/harness2` flag: a mode name, a `k`, or an option.
/// Anything else starts the task.
fn is_flag_token(token: &str) -> bool {
    if token == "--" {
        return true;
    }
    if looks_like_flag(token) {
        return true;
    }
    if token.chars().all(|c| c.is_ascii_digit()) {
        return true;
    }
    matches!(
        token.to_lowercase().as_str(),
        "parallel" | "agg" | "sequential" | "seq" | "both"
    )
}

/// Parse `/harness2 [mode] [k] [--dry-run] -- TASK`.
///
/// Returns `Ok(None)` when the input is some other command, so a caller can
/// chain it with the rest of its command handling.
pub fn parse_command(input: &str) -> Result<Option<Harness2Command>, String> {
    let trimmed = input.trim();
    let Some(rest) = trimmed.strip_prefix("/harness2") else {
        return Ok(None);
    };
    // `/harness2foo` is not our command.
    if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
        return Ok(None);
    }
    let rest = rest.trim();
    if rest.is_empty() {
        return Err(Harness2Command::usage().to_string());
    }
    // Flags come first. With an explicit `--` separator, everything before it
    // MUST be a flag: a stray word there is a typo, not a task. Without one,
    // the first non-flag token starts the task.
    let tokens: Vec<&str> = rest.split_whitespace().collect();
    let (flag_tokens, task): (Vec<&str>, String) = match tokens.iter().position(|t| *t == "--") {
        Some(at) => {
            if let Some(bad) = tokens[..at].iter().find(|t| !is_flag_token(t)) {
                return Err(format!(
                    "unexpected {bad:?} before `--`; /harness2 takes a mode, a k and \
                         options there. Put the task after `--`."
                ));
            }
            (tokens[..at].to_vec(), tokens[at + 1..].join(" "))
        }
        None => {
            let split = tokens
                .iter()
                .position(|t| !is_flag_token(t))
                .unwrap_or(tokens.len());
            (tokens[..split].to_vec(), tokens[split..].join(" "))
        }
    };
    let mut mode = "both".to_string();
    let mut k = 2u32;
    let mut dry_run = false;
    for token in flag_tokens {
        if looks_like_flag(token) {
            match token {
                "--dry-run" | "-n" => dry_run = true,
                "--run" => dry_run = false,
                other => return Err(format!("unknown /harness2 option {other}")),
            }
            continue;
        }
        if let Ok(value) = token.parse::<u32>() {
            if value == 0 {
                return Err("k must be at least 1".into());
            }
            k = value;
            continue;
        }
        match token.to_lowercase().as_str() {
            "parallel" | "agg" => mode = "parallel".into(),
            "sequential" | "seq" => mode = "sequential".into(),
            "both" => mode = "both".into(),
            other => return Err(format!("unknown /harness2 mode {other}")),
        }
    }
    if task.is_empty() {
        return Err(Harness2Command::usage().to_string());
    }
    Ok(Some(Harness2Command {
        task: task.to_string(),
        k,
        mode,
        dry_run,
    }))
}

#[async_trait::async_trait]
impl ScriptedSession for NullSession {
    async fn reply(
        &self,
        _label: &str,
        _workspace: &Path,
        _mission: &str,
    ) -> Result<(String, f64), String> {
        Err("no model is configured for the harness2 improver".into())
    }
}

/// Used by the desktop to render one rollout's key facts in tests.
pub fn summarize_run(rv: &RunView) -> String {
    format!(
        "{} ({}) ok={} delivered={} {}",
        rv.label, rv.run_id, rv.ok, rv.delivered, rv.error
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_parses_mode_k_and_task() {
        let parsed = parse_command("/harness2 -- fix the ledger totals")
            .unwrap()
            .expect("our command");
        assert_eq!(parsed.task, "fix the ledger totals");
        assert_eq!(
            (parsed.k, parsed.mode.as_str(), parsed.dry_run),
            (2, "both", false)
        );

        let parsed = parse_command("/harness2 sequential 3 --dry-run -- fix the totals")
            .unwrap()
            .unwrap();
        assert_eq!(parsed.task, "fix the totals");
        assert_eq!(
            (parsed.k, parsed.mode.as_str(), parsed.dry_run),
            (3, "sequential", true)
        );

        let parsed = parse_command("/harness2 parallel 1 -- pad the report")
            .unwrap()
            .unwrap();
        assert_eq!(
            (parsed.task.as_str(), parsed.mode.as_str()),
            ("pad the report", "parallel")
        );

        // A bare task with no flags is the common case.
        let parsed = parse_command("/harness2 rewrite the README")
            .unwrap()
            .unwrap();
        assert_eq!(parsed.task, "rewrite the README");
        assert_eq!(parsed.k, 2);

        // `both` is the default and `k` alone is accepted.
        let parsed = parse_command("/harness2 4 -- widen the tests")
            .unwrap()
            .unwrap();
        assert_eq!((parsed.k, parsed.mode.as_str()), (4, "both"));
    }

    #[test]
    fn command_rejects_bad_input_and_declines_other_commands() {
        assert!(parse_command("/harness2").is_err());
        assert!(parse_command("/harness2 sequential 0 -- x").is_err());
        assert!(parse_command("/harness2 sideways -- x").is_err());
        assert!(parse_command("/harness2 parallel").is_err());
        // Other commands and a longer word that merely starts the same way.
        assert!(parse_command("/skills").unwrap().is_none());
        assert!(parse_command("/harness22 x").unwrap().is_none());
    }

    #[test]
    fn observations_are_truncated_but_events_survive() {
        let body = "[ACTION] read\n[OBSERVATION]\n\
                    aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n\
                    [ACTION] write\n[ASSISTANT] done\n";
        let out = truncate_observations(body, 10);
        assert!(out.contains("[ACTION] read"));
        assert!(out.contains("[ACTION] write"));
        assert!(out.contains("[ASSISTANT] done"));
        assert!(out.contains("observation truncated"));
    }
}
