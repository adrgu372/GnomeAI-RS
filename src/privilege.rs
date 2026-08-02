//! Local privilege broker for the native `sudo` tool.
//!
//! Passwords never enter model messages, command arguments, environment
//! variables, temporary files or logs. The UI sends a redacted/zeroizing
//! value over an in-process channel, this broker writes it directly to the
//! child's stdin, and sudo keeps the resulting timestamp ticket.

use anyhow::{Context, Result, bail};
use std::os::unix::fs::PermissionsExt;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};
use tokio::process::Command;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

use crate::protocol::{Event, SecretString};

pub struct PrivilegeCredential {
    pub request_id: String,
    pub credential: Option<SecretString>,
    pub remember: bool,
}

#[derive(Clone)]
pub struct PrivilegeBroker {
    events: mpsc::Sender<Event>,
    replies: Arc<Mutex<mpsc::Receiver<PrivilegeCredential>>>,
}

impl PrivilegeBroker {
    pub fn new(events: mpsc::Sender<Event>, replies: mpsc::Receiver<PrivilegeCredential>) -> Self {
        Self {
            events,
            replies: Arc::new(Mutex::new(replies)),
        }
    }

    pub async fn ensure_authenticated(
        &self,
        command: &str,
        cancel: &CancellationToken,
    ) -> Result<()> {
        if validate_sudo(None, cancel).await? {
            return Ok(());
        }

        let keyring = keyring_available();
        if keyring {
            match lookup_keyring_secret(cancel).await {
                Ok(Some(secret)) => {
                    if validate_sudo(Some(&secret), cancel).await? {
                        return Ok(());
                    }
                    clear_keyring_secret().await;
                }
                Ok(None) => {}
                Err(error) => {
                    let _ = self
                        .events
                        .send(Event::Notice {
                            message: format!(
                                "desktop keyring unavailable; requesting sudo password for this session only: {error}"
                            ),
                        })
                        .await;
                }
            }
        }

        let request_id = uuid::Uuid::new_v4().simple().to_string();
        let mut message = None;
        for attempt in 1..=3 {
            self.events
                .send(Event::PrivilegeCredentialRequest {
                    request_id: request_id.clone(),
                    command: command.to_string(),
                    keyring_available: keyring,
                    attempt,
                    message: message.take(),
                })
                .await
                .context("interface disconnected while requesting sudo authentication")?;

            let reply = self.wait_for_reply(&request_id, cancel).await?;
            let Some(secret) = reply.credential else {
                bail!("sudo authentication cancelled by the user");
            };
            if validate_sudo(Some(&secret), cancel).await? {
                if reply.remember && keyring {
                    if let Err(error) = store_keyring_secret(&secret, cancel).await {
                        let _ = self
                            .events
                            .send(Event::Notice {
                                message: format!(
                                    "sudo authenticated, but the desktop keyring did not save the credential: {error}"
                                ),
                            })
                            .await;
                    }
                }
                return Ok(());
            }
            message = Some("Parola nu a fost acceptată de sudo. Încearcă din nou.".into());
        }
        bail!("sudo authentication failed after three attempts")
    }

    async fn wait_for_reply(
        &self,
        request_id: &str,
        cancel: &CancellationToken,
    ) -> Result<PrivilegeCredential> {
        let mut replies = self.replies.lock().await;
        loop {
            let reply = tokio::select! {
                biased;
                _ = cancel.cancelled() => bail!("sudo authentication cancelled"),
                reply = replies.recv() => reply,
            };
            let Some(reply) = reply else {
                bail!("sudo credential channel closed");
            };
            if reply.request_id == request_id {
                return Ok(reply);
            }
        }
    }
}

pub(crate) async fn validate_sudo(
    credential: Option<&SecretString>,
    cancel: &CancellationToken,
) -> Result<bool> {
    let mut command = Command::new("sudo");
    if credential.is_some() {
        command.args(["-S", "-p", "", "-v"]);
        command.stdin(Stdio::piped());
    } else {
        command.args(["-n", "-v"]);
        command.stdin(Stdio::null());
    }
    command
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!("sudo is not installed")
        }
        Err(error) => return Err(error).context("failed to start sudo authentication"),
    };
    if let Some(secret) = credential {
        let stdin = child.stdin.take().context("sudo stdin was unavailable")?;
        let mut writer = BufWriter::new(stdin);
        writer.write_all(secret.expose().as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await?;
        writer.shutdown().await?;
    }

    let status = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            let _ = child.kill().await;
            bail!("sudo authentication cancelled");
        }
        result = tokio::time::timeout(Duration::from_secs(20), child.wait()) => {
            match result {
                Ok(status) => status?,
                Err(_) => {
                    let _ = child.kill().await;
                    bail!("sudo authentication timed out");
                }
            }
        }
    };
    Ok(status.success())
}

pub(crate) fn keyring_available() -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|directory| {
        let candidate = directory.join("secret-tool");
        candidate
            .metadata()
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    })
}

fn keyring_attributes() -> [String; 6] {
    let user = std::env::var("USER")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| unsafe { libc::geteuid() }.to_string());
    [
        "application".into(),
        "gnomeai-rs".into(),
        "credential".into(),
        "sudo".into(),
        "user".into(),
        user,
    ]
}

pub(crate) async fn lookup_keyring_secret(
    cancel: &CancellationToken,
) -> Result<Option<SecretString>> {
    let attributes = keyring_attributes();
    let mut child = Command::new("secret-tool")
        .arg("lookup")
        .args(&attributes)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("failed to query the desktop keyring")?;
    let mut stdout = child
        .stdout
        .take()
        .context("keyring stdout was unavailable")?;
    let output_task = tokio::spawn(async move {
        let mut output = Vec::new();
        stdout.read_to_end(&mut output).await?;
        Ok::<_, std::io::Error>(output)
    });
    let status = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            let _ = child.kill().await;
            bail!("sudo authentication cancelled");
        }
        result = tokio::time::timeout(Duration::from_secs(15), child.wait()) => {
            match result {
                Ok(status) => status?,
                Err(_) => {
                    let _ = child.kill().await;
                    bail!("desktop keyring lookup timed out");
                }
            }
        }
    };
    let output = output_task.await??;
    if !status.success() || output.is_empty() {
        return Ok(None);
    }
    let mut value = String::from_utf8(output).context("keyring returned invalid UTF-8")?;
    while matches!(value.as_bytes().last(), Some(b'\n' | b'\r')) {
        value.pop();
    }
    Ok((!value.is_empty()).then(|| SecretString::new(value)))
}

pub(crate) async fn store_keyring_secret(
    secret: &SecretString,
    cancel: &CancellationToken,
) -> Result<()> {
    let attributes = keyring_attributes();
    let mut child = Command::new("secret-tool")
        .args(["store", "--label=GnomeAI sudo credential"])
        .args(&attributes)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("failed to open the desktop keyring")?;
    let stdin = child
        .stdin
        .take()
        .context("keyring stdin was unavailable")?;
    let mut writer = BufWriter::new(stdin);
    writer.write_all(secret.expose().as_bytes()).await?;
    writer.write_all(b"\n").await?;
    writer.flush().await?;
    writer.shutdown().await?;

    let status = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            let _ = child.kill().await;
            bail!("keyring storage cancelled");
        }
        result = tokio::time::timeout(Duration::from_secs(20), child.wait()) => {
            match result {
                Ok(status) => status?,
                Err(_) => {
                    let _ = child.kill().await;
                    bail!("desktop keyring storage timed out");
                }
            }
        }
    };
    if !status.success() {
        bail!("desktop keyring refused the sudo credential");
    }
    Ok(())
}

pub(crate) async fn clear_keyring_secret() {
    let attributes = keyring_attributes();
    let _ = tokio::time::timeout(
        Duration::from_secs(10),
        Command::new("secret-tool")
            .arg("clear")
            .args(&attributes)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status(),
    )
    .await;
}

/// Detect sudo only when it appears in command position. This keeps ordinary
/// text such as `echo sudo` usable while rejecting pipelines and compound
/// commands that would otherwise block on an invisible password prompt.
pub fn command_requests_privilege(command: &str) -> bool {
    let mut token = String::new();
    let mut single_quote = false;
    let mut double_quote = false;
    let mut escaped = false;
    let mut command_position = true;

    let finish = |token: &mut String, command_position: &mut bool| -> bool {
        if token.is_empty() {
            return false;
        }
        let word = std::mem::take(token);
        if *command_position {
            if word == "sudo" {
                return true;
            }
            let wrapper = matches!(word.as_str(), "command" | "exec" | "env" | "nohup")
                || (word.contains('=') && !word.starts_with('='))
                || word.starts_with('-');
            if !wrapper {
                *command_position = false;
            }
        }
        false
    };

    for character in command.chars() {
        if escaped {
            token.push(character);
            escaped = false;
            continue;
        }
        if character == '\\' && !single_quote {
            escaped = true;
            continue;
        }
        if character == '\'' && !double_quote {
            single_quote = !single_quote;
            continue;
        }
        if character == '"' && !single_quote {
            double_quote = !double_quote;
            continue;
        }
        if single_quote || double_quote {
            token.push(character);
            continue;
        }
        if character.is_whitespace() {
            if finish(&mut token, &mut command_position) {
                return true;
            }
            if character == '\n' {
                command_position = true;
            }
            continue;
        }
        if matches!(character, ';' | '&' | '|' | '(') {
            if finish(&mut token, &mut command_position) {
                return true;
            }
            command_position = true;
            continue;
        }
        token.push(character);
    }
    finish(&mut token, &mut command_position)
}

#[cfg(test)]
mod tests {
    use super::command_requests_privilege;

    #[test]
    fn detects_sudo_in_command_position() {
        assert!(command_requests_privilege("sudo apt update"));
        assert!(command_requests_privilege("true && sudo apt update"));
        assert!(command_requests_privilege("printf x | sudo tee /root/x"));
        assert!(command_requests_privilege("env FOO=bar sudo id"));
    }

    #[test]
    fn ignores_sudo_as_data() {
        assert!(!command_requests_privilege("echo sudo"));
        assert!(!command_requests_privilege("printf '%s' sudo"));
    }
}
