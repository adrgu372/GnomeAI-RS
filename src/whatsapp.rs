use std::{
    fs,
    fs::OpenOptions,
    io::{Read, Seek, SeekFrom},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, anyhow};
use qrcode::{QrCode, render::svg};
use serde_json::{Value, json};
use tokio::{
    net::TcpStream,
    process::{Child, Command},
    sync::Mutex,
    time::{Instant, sleep, timeout},
};
use tracing::warn;

use crate::{config::AppConfig, storage::AppPaths};

#[derive(Clone)]
pub struct WhatsAppBridge {
    child: Arc<Mutex<Option<Child>>>,
    http: reqwest::Client,
    last_start_error: Arc<Mutex<Option<String>>>,
}

impl WhatsAppBridge {
    pub fn new() -> Self {
        Self {
            child: Arc::new(Mutex::new(None)),
            http: reqwest::Client::new(),
            last_start_error: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn is_running(&self) -> bool {
        let mut guard = self.child.lock().await;
        let Some(child) = guard.as_mut() else {
            return false;
        };
        match child.try_wait() {
            Ok(Some(_)) => {
                *guard = None;
                false
            }
            Ok(None) => true,
            Err(err) => {
                warn!("WhatsApp bridge try_wait failed: {err}");
                true
            }
        }
    }

    pub async fn start(
        &self,
        cfg: &AppConfig,
        paths: &AppPaths,
        force_restart: bool,
    ) -> anyhow::Result<String> {
        let existing_status = self.bridge_status(cfg).await;
        let bridge_running = existing_status.is_some();
        let logged_out = existing_status
            .as_ref()
            .and_then(|status| status.get("last_error"))
            .and_then(Value::as_str)
            == Some("logged_out");

        if force_restart || logged_out {
            self.stop(cfg).await;
        } else if self.is_running().await || bridge_running {
            return Ok("already running".into());
        } else if bridge_port_in_use(cfg).await {
            let message = format!(
                "WhatsApp bridge port {} is occupied by another process",
                cfg.whatsapp_bridge_port
            );
            *self.last_start_error.lock().await = Some(message.clone());
            return Err(anyhow!(message));
        }

        if logged_out {
            clear_whatsapp_auth(paths)?;
        }

        let Some(node) = node_executable() else {
            let message = "Node.js 20 or newer was not found; restore the bundled runtime or set \
                           GNOMEF_NODE_BIN"
                .to_string();
            *self.last_start_error.lock().await = Some(message.clone());
            return Err(anyhow!(message));
        };
        if let Err(error) = ensure_supported_node(&node).await {
            let message = error.to_string();
            *self.last_start_error.lock().await = Some(message);
            return Err(error);
        }
        if !paths.whatsapp_bridge_file.exists() {
            let message = format!(
                "Missing bridge file: {}",
                paths.whatsapp_bridge_file.display()
            );
            *self.last_start_error.lock().await = Some(message.clone());
            return Err(anyhow!(message));
        }

        let log_stream = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&paths.whatsapp_log_file)
            .with_context(|| format!("failed to open {}", paths.whatsapp_log_file.display()))?;
        let log_err = log_stream.try_clone()?;
        let cwd = paths
            .whatsapp_bridge_file
            .parent()
            .and_then(Path::parent)
            .unwrap_or(&paths.app_dir);

        let mut command = Command::new(node);
        command
            .arg(&paths.whatsapp_bridge_file)
            .current_dir(cwd)
            .env(
                "GNOME_API_BASE",
                format!("http://{}:{}", cfg.host, cfg.port),
            )
            .env("GNOME_WA_BRIDGE_PORT", cfg.whatsapp_bridge_port.to_string())
            .env("GNOMEF_WEB_TOKEN", &cfg.web_api_token)
            .env("GNOME_WA_AUTH_DIR", &paths.whatsapp_auth_dir)
            .env("GNOME_WA_ASSISTANT_NAME", &cfg.whatsapp_assistant_name)
            .env(
                "GNOME_WA_HAS_OWN_NUMBER",
                if cfg.whatsapp_has_own_number {
                    "1"
                } else {
                    "0"
                },
            )
            .stdout(Stdio::from(log_stream))
            .stderr(Stdio::from(log_err))
            .kill_on_drop(false);

        let child = command.spawn().context("failed to start WhatsApp bridge")?;
        *self.child.lock().await = Some(child);
        *self.last_start_error.lock().await = None;

        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            if self.bridge_status(cfg).await.is_some() {
                return Ok("started".into());
            }
            if !self.is_running().await {
                let details = whatsapp_log_tail(&paths.whatsapp_log_file);
                let message = if details.is_empty() {
                    "WhatsApp bridge exited before its local API became ready".to_string()
                } else {
                    format!("WhatsApp bridge exited before its local API became ready:\n{details}")
                };
                *self.last_start_error.lock().await = Some(message.clone());
                return Err(anyhow!(message));
            }
            if Instant::now() >= deadline {
                self.stop(cfg).await;
                let details = whatsapp_log_tail(&paths.whatsapp_log_file);
                let message = if details.is_empty() {
                    "WhatsApp bridge did not become ready within 8 seconds".to_string()
                } else {
                    format!("WhatsApp bridge did not become ready within 8 seconds:\n{details}")
                };
                *self.last_start_error.lock().await = Some(message.clone());
                return Err(anyhow!(message));
            }
            sleep(Duration::from_millis(150)).await;
        }
    }

    /// Start a fresh pairing flow. This is intentionally separate from a
    /// normal restart: regenerating a QR invalidates stale credentials, while
    /// restarting a connected bridge must preserve the current session.
    pub async fn restart_for_pairing(
        &self,
        cfg: &AppConfig,
        paths: &AppPaths,
    ) -> anyhow::Result<String> {
        let status = self.status(cfg, paths).await;
        if status
            .get("connected")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Err(anyhow!(
                "WhatsApp is already connected; stop it before starting a new pairing"
            ));
        }

        self.stop(cfg).await;
        let deadline = Instant::now() + Duration::from_secs(4);
        while self.bridge_status(cfg).await.is_some() {
            if Instant::now() >= deadline {
                return Err(anyhow!(
                    "WhatsApp bridge did not stop before the pairing reset"
                ));
            }
            sleep(Duration::from_millis(100)).await;
        }
        clear_whatsapp_auth(paths)?;
        self.start(cfg, paths, false).await?;
        Ok("Bridge restarted with a fresh pairing session".into())
    }

    pub async fn stop(&self, cfg: &AppConfig) {
        let mut child = {
            let mut guard = self.child.lock().await;
            guard.take()
        };

        let _ = self
            .http
            .post(bridge_url(cfg, "/shutdown"))
            .header("X-Gnomef-Token", &cfg.web_api_token)
            .timeout(Duration::from_secs(1))
            .send()
            .await;

        if child.is_none() {
            return;
        }

        let Some(mut child) = child.take() else {
            *self.last_start_error.lock().await = None;
            return;
        };
        match timeout(Duration::from_secs(5), child.wait()).await {
            Ok(Ok(_)) => {}
            _ => {
                let _ = child.start_kill();
                let _ = timeout(Duration::from_secs(2), child.wait()).await;
            }
        }
        *self.last_start_error.lock().await = None;
    }

    pub async fn status(&self, cfg: &AppConfig, paths: &AppPaths) -> Value {
        let external_status = self.bridge_status(cfg).await;
        let child_running = self.is_running().await;
        let mut status = json!({
            "enabled": cfg.whatsapp_enabled,
            "bridge_running": child_running || external_status.is_some(),
            "connected": false,
            "authenticated": whatsapp_auth_file(paths).exists(),
            "assistant_name": cfg.whatsapp_assistant_name,
            "has_own_number": cfg.whatsapp_has_own_number,
            "allowed_jids": cfg.whatsapp_allowed_jids,
            "qr": "",
            "own_jid": "",
            "own_phone": "",
            "last_error": "",
        });

        if let Some(payload) = external_status {
            merge_objects(&mut status, payload);
        } else if let Some(error) = self.last_start_error.lock().await.clone() {
            status["last_error"] = json!(error);
        }

        if status
            .get("own_jid")
            .and_then(Value::as_str)
            .unwrap_or("")
            .is_empty()
        {
            status["own_jid"] = json!(self_whatsapp_jid(paths));
        }
        if !status
            .get("authenticated")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            status["authenticated"] = json!(whatsapp_auth_file(paths).exists());
        }
        status["allowed_jids"] = json!(cfg.whatsapp_allowed_jids);
        status["self_chat_only"] = json!(cfg.whatsapp_allowed_jids.is_empty());
        status["qr_available"] = json!(
            !status
                .get("qr")
                .and_then(Value::as_str)
                .unwrap_or("")
                .is_empty()
        );
        status
    }

    async fn bridge_status(&self, cfg: &AppConfig) -> Option<Value> {
        match self
            .http
            .get(bridge_url(cfg, "/status"))
            .header("X-Gnomef-Token", &cfg.web_api_token)
            .timeout(Duration::from_millis(1500))
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => response.json::<Value>().await.ok(),
            _ => None,
        }
    }
}

async fn ensure_supported_node(node: &Path) -> anyhow::Result<()> {
    let output = Command::new(node)
        .arg("--version")
        .output()
        .await
        .with_context(|| format!("failed to query Node.js at {}", node.display()))?;
    if !output.status.success() {
        return Err(anyhow!(
            "`{} --version` exited with {}",
            node.display(),
            output.status
        ));
    }
    let version = String::from_utf8_lossy(&output.stdout);
    let major = version
        .trim()
        .trim_start_matches('v')
        .split('.')
        .next()
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or_else(|| anyhow!("cannot parse Node.js version `{}`", version.trim()))?;
    if major < 20 {
        return Err(anyhow!(
            "WhatsApp bridge requires Node.js 20 or newer; found {} at {}",
            version.trim(),
            node.display()
        ));
    }
    Ok(())
}

fn whatsapp_log_tail(path: &Path) -> String {
    const LIMIT: u64 = 8 * 1024;
    let Ok(mut file) = OpenOptions::new().read(true).open(path) else {
        return String::new();
    };
    let Ok(length) = file.metadata().map(|metadata| metadata.len()) else {
        return String::new();
    };
    let start = length.saturating_sub(LIMIT);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut bytes = Vec::with_capacity((length - start) as usize);
    if file.read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    if start > 0 {
        if let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') {
            bytes.drain(..=newline);
        }
    }
    String::from_utf8_lossy(&bytes).trim().to_string()
}

pub async fn send_whatsapp_message(cfg: &AppConfig, chat_jid: &str, text: &str) {
    if chat_jid.trim().is_empty() || text.trim().is_empty() {
        return;
    }
    let payload = json!({"jid": chat_jid, "text": text});
    if let Err(err) = reqwest::Client::new()
        .post(bridge_url(cfg, "/send"))
        .header("X-Gnomef-Token", &cfg.web_api_token)
        .json(&payload)
        .send()
        .await
    {
        warn!("WhatsApp send failed: {err}");
    }
}

pub fn self_whatsapp_jid(paths: &AppPaths) -> String {
    let creds_path = whatsapp_auth_file(paths);
    let Ok(raw) = std::fs::read_to_string(creds_path) else {
        return String::new();
    };
    let Ok(creds) = serde_json::from_str::<Value>(&raw) else {
        return String::new();
    };
    let me = creds
        .get("me")
        .and_then(Value::as_object)
        .and_then(|me| me.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let phone = me
        .split(':')
        .next()
        .unwrap_or("")
        .split('@')
        .next()
        .unwrap_or("");
    if phone.is_empty() {
        String::new()
    } else {
        format!("{phone}@s.whatsapp.net")
    }
}

pub fn qr_svg(qr_data: &str) -> anyhow::Result<String> {
    let code = QrCode::new(qr_data.as_bytes())?;
    Ok(code
        .render::<svg::Color<'_>>()
        .min_dimensions(220, 220)
        .dark_color(svg::Color("#111111"))
        .light_color(svg::Color("#ffffff"))
        .build())
}

pub fn bridge_url(cfg: &AppConfig, path: &str) -> String {
    format!("http://127.0.0.1:{}{}", cfg.whatsapp_bridge_port, path)
}

fn whatsapp_auth_file(paths: &AppPaths) -> PathBuf {
    paths.whatsapp_auth_dir.join("creds.json")
}

fn clear_whatsapp_auth(paths: &AppPaths) -> anyhow::Result<()> {
    if paths.whatsapp_auth_dir.exists() {
        fs::remove_dir_all(&paths.whatsapp_auth_dir).with_context(|| {
            format!(
                "failed to remove WhatsApp auth dir {}",
                paths.whatsapp_auth_dir.display()
            )
        })?;
    }
    fs::create_dir_all(&paths.whatsapp_auth_dir).with_context(|| {
        format!(
            "failed to recreate WhatsApp auth dir {}",
            paths.whatsapp_auth_dir.display()
        )
    })?;
    Ok(())
}

fn merge_objects(target: &mut Value, patch: Value) {
    let (Some(target), Some(patch)) = (target.as_object_mut(), patch.as_object()) else {
        return;
    };
    for (key, value) in patch {
        target.insert(key.clone(), value.clone());
    }
}

fn find_executable(name: &str) -> Option<PathBuf> {
    let path = Path::new(name);
    if path.components().count() > 1 && path.is_file() {
        return Some(path.to_path_buf());
    }
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
    })
}

fn node_executable() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("GNOMEF_NODE_BIN").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(path));
    }
    if let Ok(current_exe) = std::env::current_exe() {
        for candidate in bundled_node_candidates(&current_exe) {
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let system = find_executable("node");
    if system.is_some() {
        return system;
    }
    let packaged = PathBuf::from("/usr/lib/gnomeai-rs/node/bin/node");
    packaged.is_file().then_some(packaged)
}

fn bundled_node_candidates(current_exe: &Path) -> Vec<PathBuf> {
    let Some(directory) = current_exe.parent() else {
        return Vec::new();
    };
    vec![
        directory.join("node").join("bin").join("node"),
        directory.join("libexec").join("node"),
    ]
}

async fn bridge_port_in_use(cfg: &AppConfig) -> bool {
    timeout(
        Duration::from_millis(300),
        TcpStream::connect(("127.0.0.1", cfg.whatsapp_bridge_port)),
    )
    .await
    .ok()
    .and_then(Result::ok)
    .is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qr_payload_is_rendered_as_svg() {
        let svg = qr_svg("whatsapp-test-payload").unwrap();
        assert!(svg.starts_with("<?xml"));
        assert!(svg.contains("<svg"));
        assert!(svg.contains("#111111"));
    }

    #[test]
    fn bundled_node_is_resolved_next_to_webtool_binary() {
        let candidates = bundled_node_candidates(Path::new("/usr/lib/gnomeai-rs/gnomef-web"));
        assert_eq!(
            candidates[0],
            PathBuf::from("/usr/lib/gnomeai-rs/node/bin/node")
        );
    }

    #[test]
    fn log_tail_is_bounded_and_keeps_the_latest_error() {
        let root = std::env::temp_dir().join(format!(
            "gnomeai-whatsapp-log-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&root).unwrap();
        let log = root.join("bridge.log");
        let mut contents = "very-old-unique-marker\n".to_string();
        contents.push_str(&"ordinary old line\n".repeat(600));
        contents.push_str("latest useful bridge error\n");
        fs::write(&log, contents).unwrap();

        let tail = whatsapp_log_tail(&log);
        assert!(tail.len() <= 8 * 1024);
        assert!(tail.contains("latest useful bridge error"));
        assert!(!tail.contains("very-old-unique-marker"));

        fs::remove_dir_all(root).unwrap();
    }
}
