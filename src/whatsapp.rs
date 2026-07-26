use std::{
    fs,
    fs::OpenOptions,
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
    time::timeout,
};
use tracing::warn;

use crate::{config::AppConfig, storage::AppPaths};

#[derive(Clone)]
pub struct WhatsAppBridge {
    child: Arc<Mutex<Option<Child>>>,
    http: reqwest::Client,
}

impl WhatsAppBridge {
    pub fn new() -> Self {
        Self {
            child: Arc::new(Mutex::new(None)),
            http: reqwest::Client::new(),
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
            warn!(
                "WhatsApp bridge port {} is already in use; skipping duplicate start",
                cfg.whatsapp_bridge_port
            );
            return Ok("bridge port already in use".into());
        }

        if logged_out {
            clear_whatsapp_auth(paths)?;
        }

        let node =
            find_executable("node").ok_or_else(|| anyhow!("Node.js was not found in PATH"))?;
        if !paths.whatsapp_bridge_file.exists() {
            return Err(anyhow!(
                "Missing bridge file: {}",
                paths.whatsapp_bridge_file.display()
            ));
        }

        let log_stream = OpenOptions::new()
            .create(true)
            .append(true)
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
        Ok("started".into())
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
            return;
        };
        match timeout(Duration::from_secs(5), child.wait()).await {
            Ok(Ok(_)) => {}
            _ => {
                let _ = child.start_kill();
                let _ = timeout(Duration::from_secs(2), child.wait()).await;
            }
        }
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
