//! Local control helper for delegated OpenAI/Anthropic account providers.
//! It never prints Hub credentials and intentionally cannot change root policy.

#[path = "../app_dirs.rs"]
mod app_dirs;
#[path = "../config.rs"]
mod config;
#[path = "../node_protocol.rs"]
mod node_protocol;
#[path = "../nodes.rs"]
mod nodes;
#[path = "../skills.rs"]
mod skills;
#[path = "../storage.rs"]
mod storage;

use anyhow::{Context, Result, bail};
use config::AppConfig;
use node_protocol::QueueJobRequest;
use serde::Deserialize;

#[derive(Deserialize)]
struct LearnFile {
    name: String,
    description: String,
    instructions: String,
    #[serde(default)]
    script: Option<String>,
    #[serde(default)]
    platforms: Vec<String>,
    #[serde(default)]
    replace: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.is_empty() || matches!(args[0].as_str(), "help" | "--help" | "-h") {
        print_help();
        return Ok(());
    }
    let launch_dir = std::env::current_dir()?;
    let app_home = app_dirs::resolve_app_home(&launch_dir)?;
    let config = AppConfig::load(&app_home.join("config.json"))?;
    let value = match args[0].as_str() {
        "list" => node_client(&config)?.list().await?,
        "exec" => execute(&node_client(&config)?, &args[1..]).await?,
        "learn" => learn(&args[1..])?,
        "run-skill" => run_skill(&config, &launch_dir, &args[1..]).await?,
        other => bail!("unknown command `{other}`; use `gnomeai-hubctl help`"),
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn node_client(config: &AppConfig) -> Result<nodes::NodeClient> {
    if !config.node_hub_enabled {
        bail!("Node Hub is disabled; enable it in GnomeAI Settings and restart the app")
    }
    Ok(nodes::local_client(
        config.node_hub_port,
        &config.node_hub_admin_token,
    ))
}

fn learn(args: &[String]) -> Result<serde_json::Value> {
    let path = args.first().context("usage: gnomeai-hubctl learn SPEC.json")?;
    let input: LearnFile = serde_json::from_slice(
        &std::fs::read(path).with_context(|| format!("cannot read learning spec `{path}`"))?,
    )?;
    let summary = skills::learn(skills::LearnedSkillSpec {
        name: input.name,
        description: input.description,
        instructions: input.instructions,
        script: input.script,
        platforms: input.platforms,
        replace: input.replace,
    })?;
    Ok(serde_json::json!({"ok": true, "skill": summary}))
}

async fn run_skill(
    config: &AppConfig,
    workspace: &std::path::Path,
    args: &[String],
) -> Result<serde_json::Value> {
    let name = args.first().context(
        "usage: gnomeai-hubctl run-skill NAME [--node ID] [--cwd PATH] [--root]",
    )?;
    let entrypoint = skills::entrypoint(workspace, name)?;
    let timeout_secs = flag(args, "--timeout")
        .and_then(|value| value.parse().ok())
        .unwrap_or(120)
        .clamp(1, 3_600);
    if let Some(node_id) = flag(args, "--node") {
        return node_client(config)?
            .execute(
                &node_id,
                &QueueJobRequest {
                    action: "script".into(),
                    command: String::new(),
                    stdin: entrypoint.script,
                    cwd: flag(args, "--cwd"),
                    timeout_secs,
                    root: args.iter().any(|argument| argument == "--root"),
                    root_approved: false,
                },
            )
            .await;
    }
    if args.iter().any(|argument| argument == "--root") {
        bail!("local skill root is unavailable here; use GnomeAI's dedicated Sudo tool")
    }
    let mut command = tokio::process::Command::new("/bin/sh");
    command.arg(&entrypoint.path);
    command.current_dir(
        flag(args, "--cwd")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| workspace.to_path_buf()),
    );
    command.kill_on_drop(true);
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        command.output(),
    )
    .await
    .context("skill timed out")??;
    Ok(serde_json::json!({
        "ok": output.status.success(),
        "exit_code": output.status.code(),
        "stdout": String::from_utf8_lossy(&output.stdout),
        "stderr": String::from_utf8_lossy(&output.stderr),
    }))
}

async fn execute(client: &nodes::NodeClient, args: &[String]) -> Result<serde_json::Value> {
    let separator = args
        .iter()
        .position(|argument| argument == "--")
        .context("usage: gnomeai-hubctl exec [OPTIONS] NODE_ID -- COMMAND...")?;
    let prefix = &args[..separator];
    let node_id = positional_node(prefix).context("NODE_ID is required")?;
    let command = args[separator + 1..].join(" ");
    if command.trim().is_empty() {
        bail!("COMMAND is required after --")
    }
    client
        .execute(
            node_id,
            &QueueJobRequest {
                action: "shell".into(),
                command,
                stdin: String::new(),
                cwd: flag(prefix, "--cwd"),
                timeout_secs: flag(prefix, "--timeout")
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(60)
                    .clamp(1, 3_600),
                root: prefix.iter().any(|argument| argument == "--root"),
                // Account-backed providers are delegated processes. Ordinary
                // turn approval does not become an implicit one-shot root
                // grant; root works only under Session/Always policy.
                root_approved: false,
            },
        )
        .await
}

fn flag(args: &[String], wanted: &str) -> Option<String> {
    args.iter()
        .position(|argument| argument == wanted)
        .and_then(|index| args.get(index + 1))
        .cloned()
}

fn positional_node(args: &[String]) -> Option<&String> {
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--cwd" | "--timeout" => index += 2,
            "--root" => index += 1,
            value if value.starts_with('-') => index += 1,
            _ => return args.get(index),
        }
    }
    None
}

fn print_help() {
    println!(
        "GnomeAI local Hub control\n\n\
         gnomeai-hubctl list\n\
         gnomeai-hubctl exec [--cwd PATH] [--timeout SECONDS] [--root] NODE_ID -- COMMAND...\n\n\
         gnomeai-hubctl learn SPEC.json\n\
         gnomeai-hubctl run-skill NAME [--node ID] [--cwd PATH] [--root]\n\n\
         Root policy can be changed only from the main graphical application."
    );
}
