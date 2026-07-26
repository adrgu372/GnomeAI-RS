use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use anyhow::{Context, bail};
use chrono::{DateTime, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::AppConfig;

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub app_dir: PathBuf,
    pub chats_dir: PathBuf,
    pub config_file: PathBuf,
    pub uploads_dir: PathBuf,
    pub whatsapp_media_dir: PathBuf,
    pub whatsapp_dir: PathBuf,
    pub whatsapp_auth_dir: PathBuf,
    pub whatsapp_bridge_file: PathBuf,
    pub whatsapp_log_file: PathBuf,
    pub workflow_file: PathBuf,
    pub memory_store_file: PathBuf,
    pub task_outputs_dir: PathBuf,
    pub generated_dir: PathBuf,
    pub store_dir: PathBuf,
    pub index_file: PathBuf,
}

impl AppPaths {
    pub fn new(app_dir: PathBuf) -> anyhow::Result<Self> {
        let current_dir = std::env::current_dir().unwrap_or_else(|_| app_dir.clone());
        let asset_dir = std::env::var_os("GNOMEF_RS_ASSETS").map(PathBuf::from);
        let index_file = [
            app_dir.join("index.html"),
            asset_dir
                .as_ref()
                .map(|dir| dir.join("index.html"))
                .unwrap_or_else(|| app_dir.join("index.html")),
            PathBuf::from("/usr/share/gnomeai-rs/index.html"),
            app_dir
                .parent()
                .map(|parent| parent.join("index.html"))
                .unwrap_or_else(|| app_dir.join("index.html")),
            current_dir.join("index.html"),
            current_dir.join("..").join("index.html"),
        ]
        .into_iter()
        .find(|path| path.exists())
        .map(|path| path.canonicalize().unwrap_or(path))
        .unwrap_or_else(|| app_dir.join("index.html"));
        let whatsapp_bridge_file = [
            app_dir.join("whatsapp").join("bridge.mjs"),
            asset_dir
                .as_ref()
                .map(|dir| dir.join("whatsapp").join("bridge.mjs"))
                .unwrap_or_else(|| app_dir.join("whatsapp").join("bridge.mjs")),
            PathBuf::from("/usr/share/gnomeai-rs/whatsapp/bridge.mjs"),
            app_dir
                .parent()
                .map(|parent| parent.join("whatsapp").join("bridge.mjs"))
                .unwrap_or_else(|| app_dir.join("whatsapp").join("bridge.mjs")),
            current_dir.join("whatsapp").join("bridge.mjs"),
            current_dir.join("..").join("whatsapp").join("bridge.mjs"),
        ]
        .into_iter()
        .find(|path| path.exists())
        .unwrap_or_else(|| app_dir.join("whatsapp").join("bridge.mjs"));
        let store_dir = app_dir.join("store");
        let whatsapp_dir = store_dir.join("whatsapp");

        let paths = Self {
            chats_dir: app_dir.join("chats"),
            config_file: app_dir.join("config.json"),
            uploads_dir: app_dir.join("uploads"),
            whatsapp_media_dir: app_dir.join("uploads").join("whatsapp"),
            whatsapp_auth_dir: whatsapp_dir.join("auth"),
            whatsapp_dir,
            whatsapp_bridge_file,
            whatsapp_log_file: app_dir.join("whatsapp_bridge.log"),
            workflow_file: store_dir.join("workflow.json"),
            memory_store_file: store_dir.join("memory.json"),
            task_outputs_dir: store_dir.join("task_outputs"),
            generated_dir: app_dir.join("generated"),
            store_dir,
            index_file,
            app_dir,
        };
        paths.ensure_dirs()?;
        Ok(paths)
    }

    pub fn ensure_dirs(&self) -> anyhow::Result<()> {
        for dir in [
            &self.app_dir,
            &self.chats_dir,
            &self.uploads_dir,
            &self.whatsapp_media_dir,
            &self.whatsapp_dir,
            &self.whatsapp_auth_dir,
            &self.task_outputs_dir,
            &self.generated_dir,
            &self.store_dir,
        ] {
            fs::create_dir_all(dir)
                .with_context(|| format!("failed to create {}", dir.display()))?;
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
                .with_context(|| format!("failed to protect {}", dir.display()))?;
        }
        Ok(())
    }
}

/// Atomically replace a private state file.
///
/// State can contain chat transcripts and persistent memory. Creating the
/// temporary file as `0600` prevents a permissive umask from exposing either
/// the old or new contents to other local users.
pub fn write_private(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    fs::create_dir_all(parent).with_context(|| format!("failed to create {}", parent.display()))?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("failed to protect {}", parent.display()))?;

    let temporary = parent.join(format!(".state-{}.tmp", uuid::Uuid::new_v4().simple()));
    let result = (|| -> anyhow::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .with_context(|| format!("failed to create {}", temporary.display()))?;
        file.write_all(data)?;
        file.sync_all()?;
        fs::rename(&temporary, path)
            .with_context(|| format!("failed to replace {}", path.display()))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("failed to protect {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: Value,
    pub timestamp: DateTime<Utc>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chat {
    pub id: String,
    pub title: String,
    pub created: DateTime<Utc>,
    pub messages: Vec<ChatMessage>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatListItem {
    pub id: String,
    pub title: String,
}

pub fn list_chat_files(paths: &AppPaths) -> anyhow::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in fs::read_dir(&paths.chats_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("chat_") && name.ends_with(".json"))
            || path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| ext == "json")
        {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

pub fn list_chats(paths: &AppPaths) -> anyhow::Result<Vec<ChatListItem>> {
    let mut items = Vec::new();
    for path in list_chat_files(paths)? {
        let id = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("chat")
            .to_string();
        let chat = load_chat(paths, &id)?;
        items.push(ChatListItem {
            id: chat.id,
            title: chat.title,
        });
    }
    Ok(items)
}

pub fn chat_file_path(paths: &AppPaths, cid: &str) -> anyhow::Result<PathBuf> {
    validate_chat_id(cid)?;
    Ok(paths.chats_dir.join(format!("{cid}.json")))
}

pub fn validate_chat_id(cid: &str) -> anyhow::Result<()> {
    let re = Regex::new(r"^[A-Za-z0-9_@.-]+$").unwrap();
    if cid.is_empty() || cid.contains('/') || !re.is_match(cid) {
        bail!("invalid chat id");
    }
    Ok(())
}

pub fn new_chat(paths: &AppPaths) -> anyhow::Result<Chat> {
    let mut max_num = 0_u32;
    let re = Regex::new(r"^chat_(\d+)\.json$").unwrap();
    for entry in fs::read_dir(&paths.chats_dir)? {
        let name = entry?.file_name().to_string_lossy().to_string();
        if let Some(caps) = re.captures(&name) {
            if let Ok(n) = caps[1].parse::<u32>() {
                max_num = max_num.max(n);
            }
        }
    }

    let n = max_num + 1;
    let chat = Chat {
        id: format!("chat_{n:03}"),
        title: format!("Chat {n}"),
        created: Utc::now(),
        messages: Vec::new(),
        extra: serde_json::Map::new(),
    };
    save_chat(paths, &chat)?;
    Ok(chat)
}

pub fn load_chat(paths: &AppPaths, cid: &str) -> anyhow::Result<Chat> {
    let path = chat_file_path(paths, cid)?;
    if !path.exists() {
        return Ok(Chat {
            id: cid.to_string(),
            title: cid.to_string(),
            created: Utc::now(),
            messages: Vec::new(),
            extra: serde_json::Map::new(),
        });
    }
    let raw = fs::read_to_string(&path)?;
    let mut chat: Chat = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {}", path.display()))?;
    if chat.id.is_empty() {
        chat.id = cid.to_string();
    }
    Ok(chat)
}

pub fn save_chat(paths: &AppPaths, chat: &Chat) -> anyhow::Result<()> {
    let path = chat_file_path(paths, &chat.id)?;
    let raw = serde_json::to_string_pretty(chat)?;
    write_private(&path, raw.as_bytes())
}

pub fn delete_chat(paths: &AppPaths, cid: &str) -> anyhow::Result<()> {
    let path = chat_file_path(paths, cid)?;
    if path.exists() {
        fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::unix::fs::PermissionsExt,
        sync::{LazyLock, Mutex},
    };

    static CWD_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    #[test]
    fn resolves_repo_root_index_when_data_dir_is_external() {
        let _guard = CWD_LOCK.lock().unwrap();
        let original_cwd = std::env::current_dir().unwrap();
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let project_index = manifest_dir.join("index.html");
        assert!(project_index.exists(), "missing project index.html");
        let expected_index = project_index.canonicalize().unwrap_or(project_index);

        let temp_dir = std::env::temp_dir().join(format!(
            "gnomef-rs-storage-test-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::env::set_current_dir(&manifest_dir).unwrap();
        // Pin the asset directory: it outranks any system-wide install under
        // /usr/share, which would otherwise make this test machine-dependent.
        unsafe {
            std::env::set_var("GNOMEF_RS_ASSETS", &manifest_dir);
        }
        let paths = AppPaths::new(temp_dir.clone()).unwrap();
        unsafe {
            std::env::remove_var("GNOMEF_RS_ASSETS");
        }
        assert_eq!(paths.index_file, expected_index);
        let _ = std::env::set_current_dir(original_cwd);
        let _ = fs::remove_dir_all(temp_dir);
    }

    #[test]
    fn state_directories_and_chats_are_private() {
        let temp_dir =
            std::env::temp_dir().join(format!("gnomef-private-state-{}", uuid::Uuid::new_v4()));
        let paths = AppPaths::new(temp_dir.clone()).unwrap();
        let chat = new_chat(&paths).unwrap();

        let dir_mode = fs::metadata(&temp_dir).unwrap().permissions().mode() & 0o777;
        let file_mode = fs::metadata(chat_file_path(&paths, &chat.id).unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(file_mode, 0o600);
        fs::remove_dir_all(temp_dir).unwrap();
    }
}

pub fn append_msg(
    paths: &AppPaths,
    cfg: &AppConfig,
    cid: &str,
    role: &str,
    content: Value,
    extra: serde_json::Map<String, Value>,
) -> anyhow::Result<()> {
    let mut chat = load_chat(paths, cid)?;
    chat.messages.push(ChatMessage {
        role: role.to_string(),
        content,
        timestamp: Utc::now(),
        extra,
    });

    if chat.messages.len() > cfg.max_messages {
        let start = chat.messages.len() - cfg.max_messages;
        chat.messages = chat.messages.split_off(start);
    }
    save_chat(paths, &chat)
}

pub fn clear_chat(paths: &AppPaths, cid: &str) -> anyhow::Result<()> {
    let mut chat = load_chat(paths, cid)?;
    chat.messages.clear();
    save_chat(paths, &chat)
}

pub fn build_context(history: &[ChatMessage], window: usize) -> String {
    let mut lines = Vec::new();
    let start = history.len().saturating_sub(window);
    for message in &history[start..] {
        let role = if message.role == "user" {
            "User"
        } else {
            "Assistant"
        };
        match &message.content {
            Value::Object(obj) => {
                let file_type = obj.get("type").and_then(Value::as_str).unwrap_or("unknown");
                let filename = obj
                    .get("filename")
                    .and_then(Value::as_str)
                    .unwrap_or("file");
                lines.push(format!("User: [uploaded {file_type} file: {filename}]"));
            }
            Value::String(text) => {
                if text.starts_with("[Extracted content from uploaded file:") {
                    continue;
                }
                lines.push(format!(
                    "{role}: {}",
                    text.chars().take(500).collect::<String>()
                ));
            }
            other => lines.push(format!("{role}: {}", other)),
        }
    }
    if lines.is_empty() {
        "(empty)".into()
    } else {
        lines.join("\n")
    }
}

pub fn image_meta(filename: &str, path: &Path, ocr: &str) -> Value {
    json!({
        "type": "image",
        "filename": filename,
        "path": path.to_string_lossy(),
        "ocr": ocr.chars().take(5000).collect::<String>(),
    })
}
