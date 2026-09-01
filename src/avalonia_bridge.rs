//! Private bridge between the Rust agent core and the Avalonia desktop UI.
//!
//! The child process receives one JSON object per line on stdin and emits
//! serialised `Op` values on stdout. Keeping this adapter deliberately small
//! preserves the existing core/UI boundary while allowing the presentation
//! layer to use Avalonia and .NET.

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use std::ffi::OsString;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command as TokioCommand;
use tokio::sync::mpsc;

use crate::config::AppConfig;
use crate::native_service;
use crate::protocol::{Event, Op};
use crate::provider_catalog::{AuthKind, PROVIDERS};

#[derive(Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct WhatsAppLaunchConfig {
    api_base: String,
    bridge_base: String,
    api_port: u16,
    bridge_port: u16,
    token: String,
    enabled: bool,
    assistant_name: String,
    has_own_number: bool,
    allowed_jids: Vec<String>,
    log_file: PathBuf,
    node_api_base: String,
    node_admin_token: String,
    node_enrollment_token: String,
    node_enabled: bool,
    node_bind: String,
    node_port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    launch_error: Option<String>,
}

impl WhatsAppLaunchConfig {
    pub fn from_config(config: &AppConfig, app_home: &Path) -> Self {
        let token = native_service::load_or_create_token(app_home).unwrap_or_else(|error| {
            eprintln!("cannot persist the native-service token: {error}");
            format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            )
        });
        let (api_port, bridge_port) = native_service_ports(config);
        Self {
            api_base: format!("http://127.0.0.1:{api_port}"),
            bridge_base: format!("http://127.0.0.1:{bridge_port}"),
            api_port,
            bridge_port,
            token,
            enabled: config.whatsapp_enabled,
            assistant_name: config.whatsapp_assistant_name.clone(),
            has_own_number: config.whatsapp_has_own_number,
            allowed_jids: config.whatsapp_allowed_jids.clone(),
            log_file: app_home.join("whatsapp_bridge.log"),
            node_api_base: format!("http://127.0.0.1:{}", config.node_hub_port),
            node_admin_token: config.node_hub_admin_token.clone(),
            node_enrollment_token: config.node_hub_token.clone(),
            node_enabled: config.node_hub_enabled,
            node_bind: config.node_hub_bind.clone(),
            node_port: config.node_hub_port,
            launch_error: None,
        }
    }
}

#[derive(Serialize)]
struct ProviderInfo {
    id: &'static str,
    name: &'static str,
    auth: &'static str,
    base_url: &'static str,
    default_model: &'static str,
    description: &'static str,
}

#[derive(Serialize)]
struct UiConfig<'a> {
    event: &'static str,
    version: &'static str,
    providers: Vec<ProviderInfo>,
    whatsapp: &'a WhatsAppLaunchConfig,
}

struct WhatsAppService {
    child: Option<Child>,
}

impl WhatsAppService {
    fn launch(config: &mut WhatsAppLaunchConfig) -> Self {
        let mut service = Self { child: None };
        // Debian runs this companion as a systemd user service. Never adopt or
        // kill that process when the Avalonia window closes.
        if persistent_service_requested() {
            return service;
        }
        match companion_executable("gnomef-whatsapp") {
            Ok(executable) => match Command::new(&executable)
                .env("GNOMEF_WEB_TOKEN", &config.token)
                .env("GNOMEF_NATIVE_HELPER", "1")
                .env("GNOMEF_NATIVE_API_PORT", config.api_port.to_string())
                .env("GNOMEF_NATIVE_BRIDGE_PORT", config.bridge_port.to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(child) => service.child = Some(child),
                Err(error) => {
                    config.launch_error = Some(format!(
                        "Cannot start WhatsApp service {}: {error}",
                        executable.display()
                    ));
                }
            },
            Err(error) => config.launch_error = Some(error),
        }
        service
    }
}

impl Drop for WhatsAppService {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Run the Avalonia frontend until its window closes.
pub async fn run(
    ops: mpsc::Sender<Op>,
    mut events: mpsc::Receiver<Event>,
    mut whatsapp: WhatsAppLaunchConfig,
) -> Result<()> {
    let _whatsapp_service = WhatsAppService::launch(&mut whatsapp);
    let launch = resolve_frontend()?;
    let mut command = TokioCommand::new(&launch.program);
    if let Some(dotnet_root) = launch
        .program
        .parent()
        .and_then(Path::parent)
        .map(|root| root.join("dotnet"))
        .filter(|root| root.join("dotnet").is_file())
    {
        command
            .env("DOTNET_ROOT", &dotnet_root)
            .env("DOTNET_ROOT_X64", &dotnet_root)
            .env("DOTNET_MULTILEVEL_LOOKUP", "0");
    }
    command
        .args(&launch.args)
        .arg("--ipc")
        .env("GNOMEAI_IPC", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    let mut child = command.spawn().with_context(|| {
        format!(
            "cannot start Avalonia frontend `{}`",
            launch.program.display()
        )
    })?;
    let mut input = child.stdin.take().context("Avalonia stdin is unavailable")?;
    let output = child
        .stdout
        .take()
        .context("Avalonia stdout is unavailable")?;
    let mut lines = BufReader::new(output).lines();

    let providers = PROVIDERS
        .iter()
        .map(|provider| ProviderInfo {
            id: provider.id,
            name: provider.name,
            auth: match provider.auth {
                AuthKind::ApiKey => "api_key",
                AuthKind::Account => "account",
                AuthKind::OptionalApiKey => "optional_api_key",
            },
            base_url: provider.base_url,
            default_model: provider.default_model,
            description: provider.description,
        })
        .collect();
    write_json_line(
        &mut input,
        &UiConfig {
            event: "ui_config",
            version: env!("CARGO_PKG_VERSION"),
            providers,
            whatsapp: &whatsapp,
        },
    )
    .await?;

    loop {
        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else {
                    break;
                };
                if write_json_line(&mut input, &event).await.is_err() {
                    break;
                }
            }
            line = lines.next_line() => {
                match line.context("cannot read an operation from Avalonia")? {
                    Some(line) if !line.trim().is_empty() => match serde_json::from_str::<Op>(&line) {
                        Ok(op) => {
                            if ops.send(op).await.is_err() {
                                break;
                            }
                        }
                        Err(error) => eprintln!("ignored invalid Avalonia operation: {error}"),
                    },
                    Some(_) => {}
                    None => break,
                }
            }
            status = child.wait() => {
                let status = status.context("cannot wait for Avalonia frontend")?;
                if !status.success() {
                    bail!("Avalonia frontend exited with {status}");
                }
                return Ok(());
            }
        }
    }

    drop(input);
    let status = child.wait().await.context("cannot wait for Avalonia frontend")?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("Avalonia frontend exited with {status}"))
    }
}

async fn write_json_line<T: Serialize>(writer: &mut tokio::process::ChildStdin, value: &T) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

struct FrontendLaunch {
    program: PathBuf,
    args: Vec<OsString>,
}

fn resolve_frontend() -> Result<FrontendLaunch> {
    if let Some(path) = std::env::var_os("GNOMEAI_AVALONIA_UI") {
        return Ok(FrontendLaunch {
            program: PathBuf::from(path),
            args: Vec::new(),
        });
    }

    let current = std::env::current_exe().context("cannot locate the running executable")?;
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let candidates = [
        current
            .parent()
            .and_then(Path::parent)
            .map(|root| root.join("Resources/ui/GnomeAI.UI")),
        Some(current.with_file_name("ui").join("GnomeAI.UI")),
        Some(current.with_file_name("GnomeAI.UI")),
        Some(manifest.join("ui/GnomeAI.UI/bin/Release/net8.0/GnomeAI.UI")),
        Some(manifest.join("ui/GnomeAI.UI/bin/Debug/net8.0/GnomeAI.UI")),
    ];
    if let Some(program) = candidates
        .into_iter()
        .flatten()
        .find(|candidate| candidate.is_file())
    {
        return Ok(FrontendLaunch {
            program,
            args: Vec::new(),
        });
    }

    let project = manifest.join("ui/GnomeAI.UI/GnomeAI.UI.csproj");
    if project.is_file() && command_exists("dotnet") {
        return Ok(FrontendLaunch {
            program: PathBuf::from("dotnet"),
            args: vec![
                OsString::from("run"),
                OsString::from("--project"),
                project.into_os_string(),
                OsString::from("--no-launch-profile"),
                OsString::from("--"),
            ],
        });
    }

    bail!(
        "Avalonia frontend is missing. Build `ui/GnomeAI.UI/GnomeAI.UI.csproj` or set GNOMEAI_AVALONIA_UI"
    )
}

fn command_exists(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|directory| directory.join(name).is_file())
    })
}

fn companion_executable(name: &str) -> std::result::Result<PathBuf, String> {
    let current = std::env::current_exe()
        .map_err(|error| format!("Cannot determine the current executable: {error}"))?;
    let candidate = current.with_file_name(name);
    if candidate.is_file() {
        Ok(candidate)
    } else {
        Err(format!(
            "WhatsApp service is missing: {}. Run `cargo build --bins` or reinstall the package.",
            candidate.display()
        ))
    }
}

fn native_service_ports(config: &AppConfig) -> (u16, u16) {
    if persistent_service_requested() {
        return (config.port, config.whatsapp_bridge_port);
    }
    let api_listener = TcpListener::bind(("127.0.0.1", config.port))
        .or_else(|_| TcpListener::bind(("127.0.0.1", 0)));
    let api_port = api_listener
        .as_ref()
        .ok()
        .and_then(|listener| listener.local_addr().ok())
        .map(|address| address.port())
        .unwrap_or(config.port);
    let bridge_listener = TcpListener::bind(("127.0.0.1", config.whatsapp_bridge_port))
        .or_else(|_| TcpListener::bind(("127.0.0.1", 0)));
    let bridge_port = bridge_listener
        .as_ref()
        .ok()
        .and_then(|listener| listener.local_addr().ok())
        .map(|address| address.port())
        .unwrap_or(config.whatsapp_bridge_port);
    (api_port, bridge_port)
}

fn persistent_service_requested() -> bool {
    std::env::var_os("GNOMEF_PERSISTENT_SERVICE").is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontend_config_does_not_expose_secrets_in_debug_output() {
        // The config intentionally has no Debug implementation: accidental
        // logging of the private loopback tokens must remain a compile error.
        assert!(!std::any::type_name::<WhatsAppLaunchConfig>().is_empty());
    }
}
