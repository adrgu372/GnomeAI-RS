//! Shared graphical-desktop automation for the native and WebTool agents.
//!
//! The privileged part is deliberately tiny: a shell helper invokes the
//! desktop session's screenshot and input utilities without evaluating model
//! text as a command. Rust validates and bounds every argument, stores captures
//! in GnomeAI's private generated directory, and attaches them to the next
//! model round so navigation can be visual rather than coordinate guessing.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use serde_json::{Map, Value, json};
use tokio::process::Command;
use tokio::time::{sleep, timeout};
use tokio_util::sync::CancellationToken;

const MAX_SCREENSHOT_BYTES: u64 = 20 * 1024 * 1024;

pub async fn perform(
    generated_dir: &Path,
    args: &Map<String, Value>,
    cancel: &CancellationToken,
) -> Result<Value> {
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("observe")
        .trim()
        .to_ascii_lowercase();

    if matches!(
        action.as_str(),
        "inspect" | "activate" | "set_text" | "focus"
    ) {
        return perform_semantic(&action, args, cancel).await;
    }
    let helper = desktop_helper()?;

    if action != "observe" {
        let command_args = action_arguments(&action, args)?;
        run_helper(&helper, &command_args, cancel).await?;
        let wait_ms = args
            .get("wait_ms")
            .and_then(Value::as_u64)
            .unwrap_or(350)
            .min(5_000);
        if wait_ms > 0 {
            tokio::select! {
                _ = sleep(Duration::from_millis(wait_ms)) => {}
                _ = cancel.cancelled() => bail!("desktop action was interrupted"),
            }
        }
    }

    let screenshot_after = action == "observe"
        || args
            .get("screenshot_after")
            .and_then(Value::as_bool)
            .unwrap_or(true);
    let mut result = json!({
        "action": action,
        "ok": true,
        "session_type": std::env::var("XDG_SESSION_TYPE").unwrap_or_else(|_| "unknown".into()),
    });
    if screenshot_after {
        std::fs::create_dir_all(generated_dir).with_context(|| {
            format!(
                "cannot create desktop capture directory {}",
                generated_dir.display()
            )
        })?;
        let path = generated_dir.join(format!("desktop-{}.png", uuid::Uuid::new_v4().simple()));
        run_helper(
            &helper,
            &["observe".into(), path.to_string_lossy().into_owned()],
            cancel,
        )
        .await?;
        let metadata = std::fs::metadata(&path)
            .with_context(|| format!("desktop helper did not create {}", path.display()))?;
        if metadata.len() == 0 || metadata.len() > MAX_SCREENSHOT_BYTES {
            let _ = std::fs::remove_file(&path);
            bail!(
                "desktop screenshot has an invalid size ({} bytes)",
                metadata.len()
            );
        }
        let bytes = std::fs::read(&path)?;
        let (width, height) = png_dimensions(&bytes).unwrap_or((0, 0));
        result["screenshot_path"] = json!(path);
        result["width"] = json!(width);
        result["height"] = json!(height);
        result["bytes"] = json!(bytes.len());
    }
    Ok(result)
}

#[cfg(target_os = "linux")]
async fn perform_semantic(
    action: &str,
    args: &Map<String, Value>,
    cancel: &CancellationToken,
) -> Result<Value> {
    let required_text = |name: &str, maximum: usize| -> Result<&str> {
        let value = args
            .get(name)
            .and_then(Value::as_str)
            .with_context(|| format!("semantic desktop action `{action}` requires `{name}`"))?;
        if value.chars().count() > maximum {
            bail!("desktop `{name}` is too long");
        }
        Ok(value)
    };

    let operation = async {
        match action {
            "inspect" => {
                let query = args.get("query").and_then(Value::as_str).unwrap_or("");
                let limit = args
                    .get("limit")
                    .and_then(Value::as_u64)
                    .unwrap_or(140)
                    .clamp(1, 400) as usize;
                crate::desktop_a11y::inspect(query, limit).await
            }
            "activate" => {
                let target = required_text("target", 4_096)?;
                let action_name = args.get("action_name").and_then(Value::as_str);
                crate::desktop_a11y::activate(target, action_name).await
            }
            "set_text" => {
                let target = required_text("target", 4_096)?;
                let text = required_text("text", 20_000)?;
                crate::desktop_a11y::set_text(target, text).await
            }
            "focus" => {
                let target = required_text("target", 4_096)?;
                crate::desktop_a11y::focus(target).await
            }
            _ => unreachable!(),
        }
    };
    let mut result = tokio::select! {
        result = operation => result?,
        _ = cancel.cancelled() => bail!("semantic desktop action was interrupted"),
    };

    if action != "inspect"
        && args
            .get("inspect_after")
            .and_then(Value::as_bool)
            .unwrap_or(true)
    {
        let wait_ms = args
            .get("wait_ms")
            .and_then(Value::as_u64)
            .unwrap_or(120)
            .min(5_000);
        if wait_ms > 0 {
            tokio::select! {
                _ = sleep(Duration::from_millis(wait_ms)) => {}
                _ = cancel.cancelled() => bail!("semantic desktop action was interrupted"),
            }
        }
        let query = args
            .get("after_query")
            .and_then(Value::as_str)
            .unwrap_or("");
        let updated = tokio::select! {
            result = crate::desktop_a11y::inspect(query, 100) => result?,
            _ = cancel.cancelled() => bail!("semantic desktop inspection was interrupted"),
        };
        result["updated_ui"] = updated;
    }
    Ok(result)
}

#[cfg(not(target_os = "linux"))]
async fn perform_semantic(
    _action: &str,
    _args: &Map<String, Value>,
    _cancel: &CancellationToken,
) -> Result<Value> {
    bail!("semantic desktop navigation is currently available on Linux only")
}

pub fn screenshot_path(result: &Value) -> Option<PathBuf> {
    result
        .get("screenshot_path")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .filter(|path| path.is_file())
}

pub fn screenshot_user_content(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("cannot read desktop screenshot {}", path.display()))?;
    if bytes.is_empty() || bytes.len() as u64 > MAX_SCREENSHOT_BYTES {
        bail!("desktop screenshot is empty or too large");
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    serde_json::to_string(&json!([
        {
            "type": "text",
            "text": "Current desktop screenshot after the desktop tool action. Inspect the actual image before choosing the next click, key, or text action."
        },
        {
            "type": "image_url",
            "image_url": {"url": format!("data:image/png;base64,{encoded}")}
        }
    ]))
    .context("cannot encode desktop screenshot message")
}

fn action_arguments(action: &str, args: &Map<String, Value>) -> Result<Vec<String>> {
    let integer = |name: &str, min: i64, max: i64| -> Result<i64> {
        let value = args
            .get(name)
            .and_then(Value::as_i64)
            .with_context(|| format!("desktop action `{action}` requires `{name}`"))?;
        if !(min..=max).contains(&value) {
            bail!("desktop `{name}` must be between {min} and {max}");
        }
        Ok(value)
    };
    let text = |name: &str, max_chars: usize| -> Result<String> {
        let value = args
            .get(name)
            .and_then(Value::as_str)
            .with_context(|| format!("desktop action `{action}` requires `{name}`"))?;
        if value.chars().count() > max_chars {
            bail!("desktop `{name}` is too long");
        }
        Ok(value.to_string())
    };

    match action {
        "click" | "double_click" => Ok(vec![
            action.into(),
            integer("x", 0, 32_767)?.to_string(),
            integer("y", 0, 32_767)?.to_string(),
            args.get("button")
                .and_then(Value::as_i64)
                .unwrap_or(1)
                .clamp(1, 9)
                .to_string(),
        ]),
        "move" => Ok(vec![
            action.into(),
            integer("x", 0, 32_767)?.to_string(),
            integer("y", 0, 32_767)?.to_string(),
        ]),
        "type" => Ok(vec![action.into(), text("text", 20_000)?]),
        "key" => Ok(vec![action.into(), text("keys", 200)?]),
        "scroll" => Ok(vec![action.into(), integer("amount", -50, 50)?.to_string()]),
        "focus_window" => Ok(vec![action.into(), text("window", 500)?]),
        _ => bail!(
            "unknown desktop action `{action}`; use inspect, activate, set_text, focus, observe, click, double_click, move, type, key, scroll, or focus_window"
        ),
    }
}

async fn run_helper(helper: &Path, args: &[String], cancel: &CancellationToken) -> Result<()> {
    let mut command = Command::new(helper);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::select! {
        result = timeout(Duration::from_secs(20), command.output()) => {
            result.context("desktop helper timed out")??
        }
        _ = cancel.cancelled() => bail!("desktop action was interrupted"),
    };
    if !output.status.success() {
        let error = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!(
            "desktop helper failed ({}): {}",
            output.status,
            if error.is_empty() {
                "no diagnostic output"
            } else {
                &error
            }
        );
    }
    Ok(())
}

fn desktop_helper() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("GNOMEAI_DESKTOP_HELPER")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_file())
    {
        return Ok(path);
    }
    let mut candidates = vec![
        PathBuf::from("/usr/bin/gnomeai-desktop"),
        PathBuf::from("scripts/gnomeai-desktop"),
    ];
    if let Ok(executable) = std::env::current_exe()
        && let Some(directory) = executable.parent()
    {
        candidates.push(directory.join("gnomeai-desktop"));
        candidates.push(directory.join("../share/gnomeai-rs/gnomeai-desktop"));
    }
    candidates.into_iter().find(|path| path.is_file()).context(
        "desktop automation helper is missing; reinstall GnomeAI-RS or set GNOMEAI_DESKTOP_HELPER",
    )
}

fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() < 24 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    Some((
        u32::from_be_bytes(bytes[16..20].try_into().ok()?),
        u32::from_be_bytes(bytes[20..24].try_into().ok()?),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desktop_arguments_are_bounded_and_never_shell_parsed() {
        let args = json!({"x": 120, "y": 80, "button": 1})
            .as_object()
            .unwrap()
            .clone();
        assert_eq!(
            action_arguments("click", &args).unwrap(),
            ["click", "120", "80", "1"]
        );
        let bad = json!({"amount": 500}).as_object().unwrap().clone();
        assert!(action_arguments("scroll", &bad).is_err());
    }

    #[test]
    fn png_header_dimensions_are_read_without_decoding_pixels() {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(&[0, 0, 0, 13, b'I', b'H', b'D', b'R']);
        bytes.extend_from_slice(&1920u32.to_be_bytes());
        bytes.extend_from_slice(&1080u32.to_be_bytes());
        assert_eq!(png_dimensions(&bytes), Some((1920, 1080)));
    }
}
