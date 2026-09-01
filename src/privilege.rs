//! Local privilege broker for the native `sudo` tool.
//!
//! Passwords and challenge responses never enter model messages, command
//! arguments, environment variables, temporary files or logs. The UI sends a
//! redacted/zeroizing value over an in-process channel. For a simple cached
//! keyring password it is written to sudo's stdin; interactive PAM
//! conversations use sudo's askpass protocol over a private local socket.

use anyhow::{Context, Result, bail};
use std::fs;
use std::io::{Read as _, Write as _};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Command;
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroize;

use crate::protocol::{Event, SecretString};

const ASKPASS_MODE_ENV: &str = "GNOMEF_SUDO_ASKPASS";
const ASKPASS_SOCKET_ENV: &str = "GNOMEF_SUDO_ASKPASS_SOCKET";
const ASKPASS_TOKEN_ENV: &str = "GNOMEF_SUDO_ASKPASS_TOKEN";
const MAX_ASKPASS_FRAME: usize = 16 * 1024;

pub struct PrivilegeCredential {
    pub request_id: String,
    pub credential: Option<SecretString>,
    pub remember: bool,
}

#[derive(Clone)]
pub struct PrivilegeBroker {
    events: mpsc::Sender<Event>,
    replies: Arc<Mutex<mpsc::Receiver<PrivilegeCredential>>>,
    gate: Arc<Mutex<()>>,
}

impl PrivilegeBroker {
    pub fn new(events: mpsc::Sender<Event>, replies: mpsc::Receiver<PrivilegeCredential>) -> Self {
        Self {
            events,
            replies: Arc::new(Mutex::new(replies)),
            gate: Arc::new(Mutex::new(())),
        }
    }

    pub async fn ensure_authenticated(
        &self,
        command: &str,
        cancel: &CancellationToken,
    ) -> Result<()> {
        // sudo/PAM is a desktop-global interaction. Expose one credential
        // conversation at a time so replies cannot cross between chat turns.
        let _gate = self.gate.lock().await;
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

        self.authenticate_dynamic(command, keyring, cancel).await
    }

    async fn authenticate_dynamic(
        &self,
        command_label: &str,
        keyring: bool,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let endpoint = AskpassEndpoint::create()?;
        let listener = UnixListener::bind(&endpoint.socket)
            .context("cannot create the private sudo askpass socket")?;
        fs::set_permissions(&endpoint.socket, fs::Permissions::from_mode(0o600))?;
        let executable = std::env::current_exe()
            .context("cannot locate the GnomeAI executable for sudo askpass")?;

        let mut command = Command::new("sudo");
        command
            .args(["-A", "-p", "GnomeAI administrator credential: ", "-v"])
            .env("SUDO_ASKPASS", executable)
            .env(ASKPASS_MODE_ENV, "1")
            .env(ASKPASS_SOCKET_ENV, &endpoint.socket)
            .env(ASKPASS_TOKEN_ENV, &endpoint.token)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                bail!("sudo is not installed")
            }
            Err(error) => return Err(error).context("failed to start dynamic sudo authentication"),
        };
        let mut child_task = tokio::spawn(child.wait_with_output());
        let timeout = tokio::time::sleep(Duration::from_secs(180));
        tokio::pin!(timeout);
        let mut step = 0_u8;
        let mut remembered = None;
        let mut password_only = true;

        let output = loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    child_task.abort();
                    let _ = child_task.await;
                    bail!("sudo authentication cancelled");
                }
                result = &mut child_task => {
                    break result.context("sudo authentication task failed")??;
                }
                accepted = listener.accept() => {
                    let (mut socket, _) = accepted.context("sudo askpass connection failed")?;
                    step = step.saturating_add(1);
                    if let Err(error) = self.answer_askpass(
                        &mut socket,
                        &endpoint.token,
                        command_label,
                        keyring,
                        step,
                        &mut remembered,
                        &mut password_only,
                        cancel,
                    ).await {
                        child_task.abort();
                        let _ = child_task.await;
                        return Err(error);
                    }
                }
                _ = &mut timeout => {
                    child_task.abort();
                    let _ = child_task.await;
                    bail!("dynamic sudo authentication timed out");
                }
            }
        };

        if !output.status.success() {
            let diagnostic = sudo_diagnostic(&output.stderr);
            if diagnostic.is_empty() {
                bail!("dynamic sudo authentication was refused")
            }
            bail!("dynamic sudo authentication was refused: {diagnostic}")
        }
        if password_only
            && let Some(secret) = remembered
            && let Err(error) = store_keyring_secret(&secret, cancel).await
        {
            let _ = self
                .events
                .send(Event::Notice {
                    message: format!(
                        "sudo authenticated, but the desktop keyring did not save the credential: {error}"
                    ),
                })
                .await;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn answer_askpass(
        &self,
        socket: &mut UnixStream,
        expected_token: &str,
        command: &str,
        keyring: bool,
        step: u8,
        remembered: &mut Option<SecretString>,
        password_only: &mut bool,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let token = read_async_frame(socket).await?;
        if token != expected_token.as_bytes() {
            write_askpass_cancel(socket).await?;
            return Ok(());
        }
        let prompt = String::from_utf8(read_async_frame(socket).await?)
            .context("sudo askpass sent an invalid prompt")?;
        let password_prompt = prompt_is_password(&prompt);
        if !password_prompt {
            *password_only = false;
            *remembered = None;
        }

        let request_id = uuid::Uuid::new_v4().simple().to_string();
        self.events
            .send(Event::PrivilegeCredentialRequest {
                request_id: request_id.clone(),
                command: command.to_string(),
                keyring_available: keyring && password_prompt,
                attempt: step,
                prompt: Some(prompt),
                dynamic: true,
                message: None,
            })
            .await
            .context("interface disconnected while requesting dynamic sudo authentication")?;
        let reply = self.wait_for_reply(&request_id, cancel).await?;
        let Some(secret) = reply.credential else {
            write_askpass_cancel(socket).await?;
            bail!("sudo authentication cancelled by the user");
        };
        if password_prompt {
            *remembered = (reply.remember && keyring).then(|| secret.clone());
        }
        socket.write_all(&[1]).await?;
        write_async_frame(socket, secret.expose().as_bytes()).await?;
        socket.shutdown().await?;
        Ok(())
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

/// Entry point used when sudo starts this executable through `SUDO_ASKPASS`.
/// The helper only relays one framed response from the already-running broker
/// to stdout, as required by sudo. It never starts the agent or a UI.
pub fn maybe_run_as_askpass() -> Result<bool> {
    if std::env::var_os(ASKPASS_MODE_ENV).as_deref() != Some(std::ffi::OsStr::new("1")) {
        return Ok(false);
    }
    let socket = std::env::var_os(ASKPASS_SOCKET_ENV)
        .map(PathBuf::from)
        .context("sudo askpass socket is missing")?;
    let token = std::env::var(ASKPASS_TOKEN_ENV).context("sudo askpass token is missing")?;
    let prompt = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "GnomeAI administrator credential: ".to_string());
    let mut stream = StdUnixStream::connect(&socket)
        .context("cannot connect to the GnomeAI sudo authentication broker")?;
    stream.set_read_timeout(Some(Duration::from_secs(180)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    write_sync_frame(&mut stream, token.as_bytes())?;
    write_sync_frame(&mut stream, prompt.as_bytes())?;
    stream.flush()?;

    let mut status = [0_u8; 1];
    stream.read_exact(&mut status)?;
    if status[0] != 1 {
        bail!("sudo authentication cancelled")
    }
    let mut secret = read_sync_frame(&mut stream)?;
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(&secret)?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    secret.zeroize();
    Ok(true)
}

struct AskpassEndpoint {
    directory: PathBuf,
    socket: PathBuf,
    token: String,
}

impl AskpassEndpoint {
    fn create() -> Result<Self> {
        let id = uuid::Uuid::new_v4().simple().to_string();
        let directory = std::env::temp_dir().join(format!("gnomeai-askpass-{id}"));
        fs::create_dir(&directory).context("cannot create sudo askpass directory")?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        Ok(Self {
            socket: directory.join("broker.sock"),
            directory,
            token: uuid::Uuid::new_v4().simple().to_string(),
        })
    }
}

impl Drop for AskpassEndpoint {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.socket);
        let _ = fs::remove_dir(&self.directory);
    }
}

async fn read_async_frame(stream: &mut UnixStream) -> Result<Vec<u8>> {
    let length = stream.read_u32().await? as usize;
    if length > MAX_ASKPASS_FRAME {
        bail!("sudo askpass frame is too large")
    }
    let mut value = vec![0; length];
    stream.read_exact(&mut value).await?;
    Ok(value)
}

async fn write_async_frame(stream: &mut UnixStream, value: &[u8]) -> Result<()> {
    if value.len() > MAX_ASKPASS_FRAME {
        bail!("sudo askpass response is too large")
    }
    stream.write_u32(value.len() as u32).await?;
    stream.write_all(value).await?;
    stream.flush().await?;
    Ok(())
}

async fn write_askpass_cancel(stream: &mut UnixStream) -> Result<()> {
    stream.write_all(&[0]).await?;
    stream.shutdown().await?;
    Ok(())
}

fn read_sync_frame(stream: &mut StdUnixStream) -> Result<Vec<u8>> {
    let mut encoded = [0_u8; 4];
    stream.read_exact(&mut encoded)?;
    let length = u32::from_be_bytes(encoded) as usize;
    if length > MAX_ASKPASS_FRAME {
        bail!("sudo askpass response is too large")
    }
    let mut value = vec![0; length];
    stream.read_exact(&mut value)?;
    Ok(value)
}

fn write_sync_frame(stream: &mut StdUnixStream, value: &[u8]) -> Result<()> {
    if value.len() > MAX_ASKPASS_FRAME {
        bail!("sudo askpass request is too large")
    }
    stream.write_all(&(value.len() as u32).to_be_bytes())?;
    stream.write_all(value)?;
    Ok(())
}

fn prompt_is_password(prompt: &str) -> bool {
    let prompt = prompt.to_ascii_lowercase();
    ["password", "passphrase", "parolă", "parola"]
        .iter()
        .any(|word| prompt.contains(word))
}

fn sudo_diagnostic(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(1000)
        .collect()
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
    use super::{command_requests_privilege, prompt_is_password, sudo_diagnostic};

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

    #[test]
    fn dynamic_prompts_only_offer_to_remember_passwords() {
        assert!(prompt_is_password("[sudo] password for adrian:"));
        assert!(prompt_is_password("Parolă administrator:"));
        assert!(!prompt_is_password("One-time verification code:"));
        assert!(!prompt_is_password("Touch your security key"));
    }

    #[test]
    fn sudo_diagnostics_are_compact_and_do_not_include_blank_lines() {
        assert_eq!(
            sudo_diagnostic(b"sudo: authentication failed\n\nPAM: try again\n"),
            "sudo: authentication failed PAM: try again"
        );
    }
}
