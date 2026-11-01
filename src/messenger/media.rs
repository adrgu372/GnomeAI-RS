//! Large attachments, sealed chunk by chunk.
//!
//! There is no Session Router and no swarm: the bytes travel on the same Tor
//! stream as the control message. The control message carries metadata, the
//! file digest and a random attachment key inside the ratchet envelope; that
//! key seals every chunk. Losing one chunk therefore costs one chunk, not the
//! file, and a resumed transfer can restart from any offset without ever
//! exposing plaintext on the wire.

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{BufReader, Read, Seek, SeekFrom},
    path::Path,
};

use super::crypto::{self, Key};

/// Standard tier ceiling. Both limits are local policy: no server exists to
/// enforce them, and a "Pro" build may raise them from the settings surface.
pub const STANDARD_LIMIT: u64 = 100 * 1024 * 1024;
pub const PRO_LIMIT: u64 = 2 * 1024 * 1024 * 1024;
/// 256 KiB keeps every frame small enough for a Tor stream to stay responsive
/// while still making per-chunk overhead negligible.
pub const DEFAULT_CHUNK_SIZE: u32 = 256 * 1024;
pub const MAX_CHUNK_SIZE: u32 = 1024 * 1024;

pub fn limit_for(pro: bool) -> u64 {
    if pro { PRO_LIMIT } else { STANDARD_LIMIT }
}

fn chunk_key(attachment_key: &Key, sha256_hex: &str, index: u32) -> Key {
    crypto::derive_key(
        attachment_key,
        sha256_hex.as_bytes(),
        &format!("attachment/chunk/{index}"),
    )
}

fn chunk_aad(sha256_hex: &str, index: u32, total: u32) -> Vec<u8> {
    format!("attachment/v1/{sha256_hex}/{index}/{total}").into_bytes()
}

/// A sealed attachment, ready to travel chunk by chunk.
pub struct Outgoing {
    pub path: std::path::PathBuf,
    pub file_name: String,
    pub mime: String,
    pub size_bytes: u64,
    pub sha256: String,
    pub chunks: u32,
    pub chunk_size: u32,
    key: Key,
}

impl Outgoing {
    pub fn attachment_key(&self) -> &Key {
        &self.key
    }

    /// Seal one chunk. `index` is zero-based and must be below `chunks`.
    pub fn sealed_chunk(&self, index: u32) -> Result<Vec<u8>> {
        if index >= self.chunks {
            bail!("chunk index is past the end of the file");
        }
        let start = u64::from(index) * u64::from(self.chunk_size);
        let length = (self.size_bytes - start).min(u64::from(self.chunk_size)) as usize;
        let mut file = File::open(&self.path)
            .with_context(|| format!("cannot open {}", self.path.display()))?;
        file.seek(SeekFrom::Start(start))?;
        let mut buffer = vec![0u8; length];
        file.read_exact(&mut buffer)?;
        let key = chunk_key(&self.key, &self.sha256, index);
        Ok(crypto::seal(
            &key,
            &buffer,
            &chunk_aad(&self.sha256, index, self.chunks),
        ))
    }
}

/// Prepare a file for transfer, enforcing the local tier limit.
pub fn prepare(path: &Path, chunk_size: u32, pro: bool) -> Result<Outgoing> {
    let metadata =
        std::fs::metadata(path).with_context(|| format!("cannot inspect {}", path.display()))?;
    if !metadata.is_file() {
        bail!("only files can be attached");
    }
    let limit = limit_for(pro);
    if metadata.len() > limit {
        bail!(
            "the file is larger than this account's {} MiB limit",
            limit / (1024 * 1024)
        );
    }
    let chunk_size = chunk_size.clamp(64 * 1024, MAX_CHUNK_SIZE);
    let sha256 = digest_file(path)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("attachment")
        .to_string();
    let chunks = metadata.len().div_ceil(u64::from(chunk_size)).max(1) as u32;
    Ok(Outgoing {
        path: path.to_path_buf(),
        mime: mime_guess::from_path(path)
            .first_raw()
            .unwrap_or("application/octet-stream")
            .to_string(),
        file_name,
        size_bytes: metadata.len(),
        sha256,
        chunks,
        chunk_size,
        key: crypto::random_key(),
    })
}

pub fn digest_file(path: &Path) -> Result<String> {
    let mut reader = BufReader::new(
        File::open(path).with_context(|| format!("cannot open {}", path.display()))?,
    );
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Collects arriving chunks and verifies the digest before the UI can offer the
/// file. An incomplete or corrupted transfer never becomes visible as a file.
pub struct Incoming {
    pub sha256: String,
    pub size_bytes: u64,
    pub chunks: u32,
    pub chunk_size: u32,
    key: Key,
    received: Vec<bool>,
    buffer: Vec<u8>,
    count: u32,
}

impl Incoming {
    pub fn new(sha256: String, size_bytes: u64, chunks: u32, chunk_size: u32, key: Key) -> Self {
        Self {
            sha256,
            size_bytes,
            chunks,
            chunk_size,
            key,
            received: vec![false; chunks as usize],
            buffer: vec![0u8; size_bytes as usize],
            count: 0,
        }
    }

    pub fn progress(&self) -> u32 {
        self.count
    }

    pub fn complete(&self) -> bool {
        self.count == self.chunks
    }

    /// Returns true when this chunk was new.
    pub fn accept(&mut self, index: u32, sealed: &[u8]) -> Result<bool> {
        if index >= self.chunks {
            bail!("a chunk arrived with an index outside the file");
        }
        if self.received[index as usize] {
            return Ok(false);
        }
        let key = chunk_key(&self.key, &self.sha256, index);
        let plaintext = crypto::open(&key, sealed, &chunk_aad(&self.sha256, index, self.chunks))?;
        let start = index as usize * self.chunk_size as usize;
        let end = start + plaintext.len();
        if end > self.buffer.len() {
            bail!("a chunk overran the declared file size");
        }
        self.buffer[start..end].copy_from_slice(&plaintext);
        self.received[index as usize] = true;
        self.count += 1;
        Ok(true)
    }

    /// Verify and atomically publish the assembled file.
    pub fn finish(self, destination: &Path) -> Result<()> {
        if !self.complete() {
            bail!("the transfer is not complete");
        }
        let actual = crypto::sha256_hex(&self.buffer);
        if !crypto::constant_time_eq(actual.as_bytes(), self.sha256.as_bytes()) {
            bail!("the received file failed its digest check");
        }
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temporary = destination.with_extension("part");
        std::fs::write(&temporary, &self.buffer)?;
        std::fs::rename(&temporary, destination)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(bytes: &[u8]) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("messenger-media-{}.bin", crypto::random_id()));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn chunks_round_trip_and_verify() {
        let payload: Vec<u8> = (0..5000u32).map(|n| (n % 251) as u8).collect();
        let path = temp_file(&payload);
        let outgoing = prepare(&path, 64 * 1024, false).unwrap();
        assert_eq!(outgoing.chunks, 1);
        let mut incoming = Incoming::new(
            outgoing.sha256.clone(),
            outgoing.size_bytes,
            outgoing.chunks,
            outgoing.chunk_size,
            *outgoing.attachment_key(),
        );
        for index in 0..outgoing.chunks {
            incoming
                .accept(index, &outgoing.sealed_chunk(index).unwrap())
                .unwrap();
        }
        let destination = path.with_extension("out");
        incoming.finish(&destination).unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), payload);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&destination);
    }

    #[test]
    fn multi_chunk_transfer_reassembles_in_order() {
        let payload: Vec<u8> = (0..300_000u32).map(|n| (n % 253) as u8).collect();
        let path = temp_file(&payload);
        let outgoing = prepare(&path, 64 * 1024, false).unwrap();
        assert!(outgoing.chunks > 3);
        let mut incoming = Incoming::new(
            outgoing.sha256.clone(),
            outgoing.size_bytes,
            outgoing.chunks,
            outgoing.chunk_size,
            *outgoing.attachment_key(),
        );
        // Deliver backwards to prove order independence.
        for index in (0..outgoing.chunks).rev() {
            incoming
                .accept(index, &outgoing.sealed_chunk(index).unwrap())
                .unwrap();
        }
        let destination = path.with_extension("out");
        incoming.finish(&destination).unwrap();
        assert_eq!(std::fs::read(&destination).unwrap(), payload);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&destination);
    }

    #[test]
    fn a_corrupted_chunk_is_rejected() {
        let path = temp_file(b"attachment contents");
        let outgoing = prepare(&path, 64 * 1024, false).unwrap();
        let mut sealed = outgoing.sealed_chunk(0).unwrap();
        let last = sealed.len() - 1;
        sealed[last] ^= 0xff;
        let mut incoming = Incoming::new(
            outgoing.sha256.clone(),
            outgoing.size_bytes,
            outgoing.chunks,
            outgoing.chunk_size,
            *outgoing.attachment_key(),
        );
        assert!(incoming.accept(0, &sealed).is_err());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_tier_limit_is_local_policy() {
        assert_eq!(limit_for(false), STANDARD_LIMIT);
        assert!(limit_for(true) > limit_for(false));
    }
}
