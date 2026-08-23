//! Minimal GnomeAI execution node. It runs on weak machines and connects
//! outbound to the main Hub without requiring systemd or any particular init.

#[path = "../node_protocol.rs"]
mod node_protocol;

use anyhow::{Context, Result, bail};
use node_protocol::{NODE_PROTOCOL_VERSION, NodeHello, NodeJob, NodePoll, NodePollResponse, NodeResult};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{io::AsyncWriteExt, process::Command};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct NodeConfig {
    server: String,
    token: String,
    node_id: String,
    name: String,
    #[serde(default)]
    allow_root: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    match args.first().map(String::as_str) {
        Some("enroll") => enroll(&args[1..]).await,
        Some("run") => run(load_with_overrides(&args[1..])?).await,
        Some("help") | Some("--help") | Some("-h") => {
            print_help();
            Ok(())
        }
        Some(other) if other.starts_with('-') => run(load_with_overrides(&args)?).await,
        None => run(load_config()?).await,
        Some(other) => bail!("unknown command `{other}`; use `gnomeai-node help`"),
    }
}

async fn enroll(args: &[String]) -> Result<()> {
    let mut config = NodeConfig {
        server: required_flag(args, "--server")?,
        token: required_flag(args, "--token")?,
        node_id: flag(args, "--id").unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string()),
        name: flag(args, "--name").unwrap_or_else(default_hostname),
        allow_root: args.iter().any(|arg| arg == "--allow-root"),
    };
    normalize_config(&mut config)?;
    save_config(&config)?;
    register(&reqwest::Client::new(), &config).await?;
    println!(
        "Node `{}` enrolled. Start it with: gnomeai-node run",
        config.name
    );
    Ok(())
}

fn load_with_overrides(args: &[String]) -> Result<NodeConfig> {
    let mut config = load_config().unwrap_or_else(|_| NodeConfig {
        server: String::new(),
        token: String::new(),
        node_id: uuid::Uuid::new_v4().simple().to_string(),
        name: default_hostname(),
        allow_root: false,
    });
    if let Some(value) = flag(args, "--server") {
        config.server = value;
    }
    if let Some(value) = flag(args, "--token") {
        config.token = value;
    }
    if let Some(value) = flag(args, "--id") {
        config.node_id = value;
    }
    if let Some(value) = flag(args, "--name") {
        config.name = value;
    }
    if args.iter().any(|arg| arg == "--allow-root") {
        config.allow_root = true;
    }
    normalize_config(&mut config)?;
    Ok(config)
}

async fn run(config: NodeConfig) -> Result<()> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(45))
        .build()?;
    let mut backoff = 1u64;
    loop {
        match run_connected(&client, &config).await {
            Ok(()) => backoff = 1,
            Err(error) => {
                eprintln!("GnomeAI node disconnected: {error}");
                tokio::time::sleep(Duration::from_secs(backoff)).await;
                backoff = (backoff * 2).min(30);
            }
        }
    }
}

async fn run_connected(client: &reqwest::Client, config: &NodeConfig) -> Result<()> {
    register(client, config).await?;
    println!(
        "GnomeAI node `{}` connected to {}",
        config.name, config.server
    );
    loop {
        let response = send_json(
            client
                .post(format!("{}/v1/poll", config.server))
                .json(&NodePoll {
                    node_id: config.node_id.clone(),
                }),
            &config.token,
        )
        .await?;
        let poll: NodePollResponse = serde_json::from_value(response)?;
        let Some(job) = poll.job else {
            continue;
        };
        let result = execute_job(config, job).await;
        send_json(
            client
                .post(format!("{}/v1/result", config.server))
                .json(&result),
            &config.token,
        )
        .await?;
    }
}

async fn register(client: &reqwest::Client, config: &NodeConfig) -> Result<()> {
    let hello = NodeHello {
        protocol: NODE_PROTOCOL_VERSION,
        node_id: config.node_id.clone(),
        name: config.name.clone(),
        hostname: default_hostname(),
        os: std::env::consts::OS.into(),
        arch: std::env::consts::ARCH.into(),
        version: env!("CARGO_PKG_VERSION").into(),
        init_system: detect_init(),
        capabilities: capabilities(),
        root_available: unsafe { libc::geteuid() } == 0
            || command_exists("sudo")
            || command_exists("doas"),
    };
    send_json(
        client
            .post(format!("{}/v1/register", config.server))
            .json(&hello),
        &config.token,
    )
    .await?;
    Ok(())
}

async fn execute_job(config: &NodeConfig, job: NodeJob) -> NodeResult {
    let started = Instant::now();
    let outcome = execute_process(config, &job).await;
    match outcome {
        Ok(output) => NodeResult {
            node_id: config.node_id.clone(),
            job_id: job.job_id,
            ok: output.status.success(),
            exit_code: output.status.code(),
            stdout: cap_bytes(&output.stdout),
            stderr: cap_bytes(&output.stderr),
            duration_ms: started.elapsed().as_millis() as u64,
        },
        Err(error) => NodeResult {
            node_id: config.node_id.clone(),
            job_id: job.job_id,
            ok: false,
            exit_code: None,
            stdout: String::new(),
            stderr: error.to_string(),
            duration_ms: started.elapsed().as_millis() as u64,
        },
    }
}

async fn execute_process(config: &NodeConfig, job: &NodeJob) -> Result<std::process::Output> {
    if job.root && !config.allow_root {
        bail!(
            "root is locally disabled; enroll this node again with --allow-root after reviewing the Hub policy"
        )
    }
    let mut command = privileged_shell(job.root)?;
    match job.action.as_str() {
        "shell" => {
            command.args(["-lc", &job.command]);
            command.stdin(Stdio::null());
        }
        "script" => {
            command.arg("-s");
            command.stdin(Stdio::piped());
        }
        other => bail!("unsupported node action `{other}`"),
    }
    if let Some(cwd) = &job.cwd {
        command.current_dir(cwd);
    }
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().context("cannot start node job")?;
    let writer = if job.action == "script" {
        let mut stdin = child.stdin.take().context("script stdin unavailable")?;
        let script = job.stdin.clone();
        Some(tokio::spawn(async move {
            stdin.write_all(script.as_bytes()).await?;
            stdin.shutdown().await
        }))
    } else {
        None
    };
    let output = tokio::time::timeout(
        Duration::from_secs(job.timeout_secs.clamp(1, 3_600)),
        child.wait_with_output(),
    )
    .await
    .context("node job timed out")??;
    if let Some(writer) = writer {
        writer.await??;
    }
    Ok(output)
}

fn privileged_shell(root: bool) -> Result<Command> {
    if !root || unsafe { libc::geteuid() } == 0 {
        return Ok(Command::new("/bin/sh"));
    }
    if command_exists("sudo") {
        let mut command = Command::new("sudo");
        command.args(["-n", "/bin/sh"]);
        return Ok(command);
    }
    if command_exists("doas") {
        let mut command = Command::new("doas");
        command.args(["-n", "/bin/sh"]);
        return Ok(command);
    }
    bail!("root requires a root-run node or pre-authorized sudo/doas")
}

async fn send_json(request: reqwest::RequestBuilder, token: &str) -> Result<Value> {
    let response = request
        .header("X-GnomeAI-Node-Token", token)
        .send()
        .await
        .context("cannot reach GnomeAI Hub")?;
    let status = response.status();
    let value: Value = response.json().await.context("Hub returned invalid JSON")?;
    if !status.is_success() {
        bail!(
            "{}",
            value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("Hub request failed")
        )
    }
    Ok(value)
}

fn config_path() -> Result<PathBuf> {
    let root = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .context("HOME or absolute XDG_CONFIG_HOME is required")?;
    Ok(root.join("gnomeai-node/config.json"))
}

fn save_config(config: &NodeConfig) -> Result<()> {
    let path = config_path()?;
    let parent = path.parent().context("invalid node config path")?;
    fs::create_dir_all(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)?;
    file.write_all(&serde_json::to_vec_pretty(config)?)?;
    file.sync_all()?;
    println!("Saved {}", path.display());
    Ok(())
}

fn load_config() -> Result<NodeConfig> {
    let path = config_path()?;
    let mut config: NodeConfig = serde_json::from_slice(
        &fs::read(&path).with_context(|| {
            format!(
                "node is not enrolled; run `gnomeai-node enroll --server URL --token TOKEN` ({})",
                path.display()
            )
        })?,
    )?;
    normalize_config(&mut config)?;
    Ok(config)
}

fn normalize_config(config: &mut NodeConfig) -> Result<()> {
    config.server = config.server.trim().trim_end_matches('/').to_string();
    config.token = config.token.trim().to_string();
    config.node_id = config.node_id.trim().to_string();
    config.name = config.name.trim().to_string();
    if !(config.server.starts_with("http://") || config.server.starts_with("https://")) {
        bail!("--server must begin with http:// or https://")
    }
    if config.token.chars().count() < 32 {
        bail!("Hub token must contain at least 32 characters")
    }
    if config.node_id.is_empty()
        || config.node_id.len() > 96
        || !config
            .node_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        bail!("invalid node id")
    }
    if config.name.is_empty() || config.name.chars().count() > 128 {
        bail!("node name must contain 1–128 characters")
    }
    Ok(())
}

fn flag(args: &[String], wanted: &str) -> Option<String> {
    args.iter()
        .position(|arg| arg == wanted)
        .and_then(|index| args.get(index + 1))
        .cloned()
}

fn required_flag(args: &[String], wanted: &str) -> Result<String> {
    flag(args, wanted).with_context(|| format!("{wanted} is required"))
}

fn default_hostname() -> String {
    std::env::var("HOSTNAME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| fs::read_to_string("/etc/hostname").ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "gnomeai-node".into())
}

fn detect_init() -> String {
    if Path::new("/run/systemd/system").exists() {
        "systemd".into()
    } else if command_exists("sv") {
        "runit".into()
    } else if command_exists("rc-service") {
        "openrc".into()
    } else if command_exists("s6-rc") || command_exists("s6-svc") {
        "s6".into()
    } else {
        "unknown/manual".into()
    }
}

fn capabilities() -> Vec<String> {
    let mut values = vec!["shell".into(), "files".into(), "process".into()];
    if command_exists("sv")
        || command_exists("systemctl")
        || command_exists("rc-service")
        || command_exists("s6-svc")
    {
        values.push("service-control".into());
    }
    if std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some() {
        values.push("desktop".into());
    }
    values
}

fn command_exists(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|directory| {
        directory.join(name).metadata().is_ok_and(|metadata| {
            metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
        })
    })
}

fn cap_bytes(bytes: &[u8]) -> String {
    const LIMIT: usize = 1_000_000;
    if bytes.len() <= LIMIT {
        String::from_utf8_lossy(bytes).into_owned()
    } else {
        format!(
            "{}\n[output truncated]",
            String::from_utf8_lossy(&bytes[..LIMIT])
        )
    }
}

fn print_help() {
    println!(
        "GnomeAI minimal node\n\n\
         Enroll: gnomeai-node enroll --server http://PC:39176 --token TOKEN [--name NAME] [--allow-root]\n\
         Run:    gnomeai-node run\n\n\
         The process runs in the foreground and works with runit, OpenRC, s6, systemd,\n\
         another supervisor, or no init integration at all."
    );
}
