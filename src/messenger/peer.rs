//! Tor plumbing for the messenger.
//!
//! The messenger reuses the embedded Arti client the device mesh already runs,
//! but never its channels or its onion identity: every messenger account asks
//! for its *own* onion service under a channel derived from the account id, so
//! two accounts on one machine have two unrelated onions, and neither can be
//! traced back to the mesh onion of the same install.
//!
//! Framing is a 4-byte big-endian length followed by that many bytes of JSON,
//! which matches `PeerTransport.cs` and keeps one huge attachment chunk from
//! being read into memory by accident.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::watch,
};

use super::wire::{MAX_FRAME, Reply, Request};

/// Arti's onion nickname/channel field is fixed-width; this keeps us inside it.
const CHANNEL_LEN: usize = 43;

/// The messenger's view of the embedded Tor client.
///
/// It is a trait so the messenger can be exercised end to end without a live
/// Tor network: a test double publishes a loopback "onion" and the rest of the
/// code path — framing, X3DH, ratchet, fan-out, queueing — is the real one.
pub trait MeshHandle: Send + Sync {
    /// `mesh_start`, `mesh_status` or `mesh_revoke`, with the same payloads the
    /// device mesh already accepts.
    fn request(&self, action: &str, payload: &serde_json::Value) -> Result<serde_json::Value>;
}

impl MeshHandle for crate::mesh_transport::Mesh {
    fn request(&self, action: &str, payload: &serde_json::Value) -> Result<serde_json::Value> {
        crate::mesh_transport::Mesh::request(self, action, payload)
    }
}

impl MeshHandle for std::sync::Arc<crate::mesh_transport::Mesh> {
    fn request(&self, action: &str, payload: &serde_json::Value) -> Result<serde_json::Value> {
        crate::mesh_transport::Mesh::request(self.as_ref(), action, payload)
    }
}

/// Stable channel per messenger account. Deterministic so the onion survives a
/// restart, and unrelated to the mesh channel of the same installation.
pub fn channel_for(account_id: &str) -> String {
    use base64::Engine as _;
    let digest =
        super::crypto::sha256(format!("gnomeai-messenger-onion/v1/{account_id}").as_bytes());
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    encoded[..CHANNEL_LEN].to_string()
}

/// Read one framed JSON message, refusing anything above the limit before it is
/// allocated.
pub async fn read_frame(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut header = [0u8; 4];
    stream.read_exact(&mut header).await?;
    let size = u32::from_be_bytes(header) as usize;
    if size == 0 || size > MAX_FRAME {
        bail!("the peer sent an invalid frame length");
    }
    let mut payload = vec![0u8; size];
    stream.read_exact(&mut payload).await?;
    Ok(payload)
}

pub async fn write_frame(stream: &mut TcpStream, payload: &[u8]) -> Result<()> {
    if payload.is_empty() || payload.len() > MAX_FRAME {
        bail!("refusing to send an oversized messenger frame");
    }
    stream
        .write_all(&(payload.len() as u32).to_be_bytes())
        .await?;
    stream.write_all(payload).await?;
    stream.flush().await?;
    Ok(())
}

/// Minimal SOCKS5 CONNECT through the local Arti proxy. Only `.onion` hosts on
/// port 80 are ever requested, so the messenger cannot be pointed at the open
/// internet through this path.
pub async fn socks_connect(socks_port: u16, onion: &str, timeout: Duration) -> Result<TcpStream> {
    if !valid_onion_host(onion) {
        bail!("a messenger peer must advertise a valid onion address");
    }
    let attempt = async {
        let mut stream = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, socks_port)).await?;
        stream.write_all(&[5, 1, 0]).await?;
        let mut greeting = [0u8; 2];
        stream.read_exact(&mut greeting).await?;
        if greeting != [5, 0] {
            bail!("the local Tor proxy refused the connection");
        }
        let host = onion.as_bytes();
        if host.len() > 255 {
            bail!("the onion address is too long");
        }
        let mut request = Vec::with_capacity(7 + host.len());
        request.extend_from_slice(&[5, 1, 0, 3, host.len() as u8]);
        request.extend_from_slice(host);
        request.extend_from_slice(&80u16.to_be_bytes());
        stream.write_all(&request).await?;
        let mut reply = [0u8; 4];
        stream.read_exact(&mut reply).await?;
        if reply[0] != 5 {
            bail!("the local Tor proxy answered with a malformed reply");
        }
        if reply[1] != 0 {
            bail!("the onion service could not be reached");
        }
        // Drain the bound address; its shape varies by address type.
        match reply[3] {
            1 => {
                let mut rest = [0u8; 6];
                stream.read_exact(&mut rest).await?;
            }
            3 => {
                let mut length = [0u8; 1];
                stream.read_exact(&mut length).await?;
                let mut rest = vec![0u8; length[0] as usize + 2];
                stream.read_exact(&mut rest).await?;
            }
            4 => {
                let mut rest = [0u8; 18];
                stream.read_exact(&mut rest).await?;
            }
            _ => bail!("the local Tor proxy answered with an unknown address type"),
        }
        Ok::<_, anyhow::Error>(stream)
    };
    tokio::time::timeout(timeout, attempt)
        .await
        .map_err(|_| anyhow::anyhow!("the onion service did not answer in time"))?
}

/// A bare 56-character v3 onion address, without the `.onion` suffix.
pub fn valid_onion_host(host: &str) -> bool {
    let host = host.trim_end_matches(".onion");
    host.len() == 56
        && host
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || (b'2'..=b'7').contains(&byte))
}

/// One request/reply exchange over a fresh stream. Short-lived by design: a
/// failed circuit costs one reconnect, and nothing is left half-open.
pub async fn exchange(
    socks_port: u16,
    onion: &str,
    request: &Request,
    timeout: Duration,
) -> Result<Reply> {
    let mut stream = socks_connect(socks_port, onion, timeout).await?;
    let encoded = serde_json::to_vec(request)?;
    write_frame(&mut stream, &encoded).await?;
    let reply = read_frame(&mut stream).await?;
    super::wire::decode_reply(std::str::from_utf8(&reply)?)
}

/// Send several requests over one circuit and collect every reply.
///
/// Opening a Tor circuit to an onion costs seconds, and it is paid *per
/// message* by `exchange`: a heartbeat that pings five contacts builds five
/// circuits, and a burst of queued messages builds one each. The protocol
/// already frames requests one after another on a stream and answers each in
/// order, so a batch can share one.
///
/// The caller keeps the usual per-request failure handling: a `Reply::Error` is
/// returned in place of the success reply for that request, and a transport
/// failure fails the whole batch — which is right, because a broken circuit
/// invalidates every request still on it.
pub async fn exchange_batch(
    socks_port: u16,
    onion: &str,
    requests: &[Request],
    timeout: Duration,
) -> Result<Vec<Reply>> {
    if requests.is_empty() {
        return Ok(Vec::new());
    }
    if requests.len() == 1 {
        return Ok(vec![
            exchange(socks_port, onion, &requests[0], timeout).await?,
        ]);
    }
    let mut stream = socks_connect(socks_port, onion, timeout).await?;
    let mut replies = Vec::with_capacity(requests.len());
    for request in requests {
        let encoded = serde_json::to_vec(request)?;
        write_frame(&mut stream, &encoded).await?;
        let frame = read_frame(&mut stream).await?;
        replies.push(super::wire::decode_reply(std::str::from_utf8(&frame)?)?);
    }
    Ok(replies)
}

/// Announces the account's onion to whoever wired this listener up.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnionState {
    pub account_id: String,
    pub onion: Option<String>,
    pub error: Option<String>,
}

/// Binds the loopback port that the account's onion service forwards to.
///
/// Arti terminates the Tor circuit and hands us a plain local socket, so the
/// messenger never speaks the Tor protocol itself and never opens an inbound
/// port beyond loopback.
pub async fn bind_listener() -> Result<(TcpListener, u16)> {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    let port = listener.local_addr()?.port();
    Ok((listener, port))
}

/// Accept loop for one account. Each connection is handled on its own task so
/// a slow or hostile peer cannot stall the account.
pub fn spawn_accept<F, Fut>(listener: TcpListener, stop: watch::Receiver<bool>, handle: F)
where
    F: Fn(TcpStream) -> Fut + Send + Sync + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let handle = std::sync::Arc::new(handle);
    tokio::spawn(async move {
        let mut stop = stop;
        loop {
            let incoming = tokio::select! {
                _ = stop.changed() => break,
                incoming = listener.accept() => incoming,
            };
            let Ok((stream, _)) = incoming else {
                break;
            };
            let handle = handle.clone();
            tokio::spawn(async move {
                handle(stream).await;
            });
        }
    });
}

/// Drive a single inbound connection: decode frames until the peer stops
/// talking, answering every one.
pub fn spawn_connection<F>(stream: TcpStream, answer: F)
where
    F: Fn(Request) -> Reply + Send + Sync + 'static,
{
    tokio::spawn(async move {
        let mut stream = stream;
        // A peer that connects and then says nothing must not hold a slot.
        loop {
            let Ok(frame) =
                tokio::time::timeout(Duration::from_secs(180), read_frame(&mut stream)).await
            else {
                break;
            };
            let Ok(frame) = frame else {
                break;
            };
            let Ok(text) = std::str::from_utf8(&frame) else {
                break;
            };
            let reply = match super::wire::decode_request(text) {
                Ok(request) => answer(request),
                Err(error) => Reply::Error {
                    message: error.to_string(),
                },
            };
            let Ok(encoded) = serde_json::to_vec(&reply) else {
                break;
            };
            if write_frame(&mut stream, &encoded).await.is_err() {
                break;
            }
        }
    });
}

/// Poll the shared Arti state until this account's onion is published.
pub async fn await_onion(mesh: &dyn MeshHandle, channel: &str, budget: Duration) -> Result<String> {
    let deadline = std::time::Instant::now() + budget;
    loop {
        let state = mesh.request("mesh_status", &serde_json::Value::Null)?;
        if let Some(onion) = state["onions"]
            .get(channel)
            .and_then(|value| value.as_str())
        {
            return Ok(onion.to_string());
        }
        if let Some(status) = state["status"].as_str() {
            if status.starts_with("Tor failed") {
                bail!("{status}");
            }
        }
        if std::time::Instant::now() >= deadline {
            bail!("Tor did not publish this account's onion service in time");
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Convenience for tests and for callers that only need the SOCKS port.
pub async fn socks_port(mesh: &dyn MeshHandle) -> Result<u16> {
    let state = mesh.request("mesh_status", &serde_json::Value::Null)?;
    let port = state["socks_port"].as_u64().unwrap_or(0);
    if port == 0 || port > u64::from(u16::MAX) {
        bail!("Tor is still starting; the messenger cannot dial yet");
    }
    Ok(port as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_channel_is_stable_and_account_scoped() {
        let first = channel_for("account-one");
        assert_eq!(first, channel_for("account-one"));
        assert_ne!(first, channel_for("account-two"));
        assert_eq!(first.len(), CHANNEL_LEN);
        assert!(
            first
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        );
    }

    #[test]
    fn onion_hosts_are_validated() {
        let good = "a".repeat(52) + "2345";
        assert!(valid_onion_host(&good));
        assert!(valid_onion_host(&format!("{good}.onion")));
        assert!(!valid_onion_host("example.com"));
        assert!(!valid_onion_host(&"a".repeat(55)));
        assert!(!valid_onion_host(&"1".repeat(56)));
    }

    #[tokio::test]
    async fn framing_round_trips_and_rejects_a_bad_length() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let client = tokio::spawn(async move {
            let mut stream = TcpStream::connect(address).await.unwrap();
            write_frame(&mut stream, b"{\"request\":\"ping\"}")
                .await
                .unwrap();
            // A zero length must be rejected rather than read as an empty frame.
            stream.write_all(&0u32.to_be_bytes()).await.unwrap();
            stream
        });
        let (mut server, _) = listener.accept().await.unwrap();
        let frame = read_frame(&mut server).await.unwrap();
        assert_eq!(frame, b"{\"request\":\"ping\"}");
        assert!(read_frame(&mut server).await.is_err());
        let _ = client.await;
    }

    #[tokio::test]
    async fn an_oversized_frame_is_refused_before_it_is_written() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let (mut client, _) = tokio::join!(
            async { TcpStream::connect(address).await.unwrap() },
            async { listener.accept().await.unwrap() }
        );
        let huge = vec![0u8; MAX_FRAME + 1];
        assert!(write_frame(&mut client, &huge).await.is_err());
    }

    #[tokio::test]
    async fn a_non_onion_host_is_never_dialled() {
        let error = socks_connect(1, "example.com", Duration::from_millis(50))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("onion"));
    }
}
