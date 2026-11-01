//! Primitives for the messenger: AEAD, X3DH, ratchet KDFs and sender chains.
//!
//! Everything here is independent of the device-mesh keys by construction: the
//! messenger never receives a device key and never derives one from an identity
//! key. `Identity` is created from its own random seed, and the master key that
//! seals it lives under a separate keystore alias.

use anyhow::{Result, bail};
use chacha20::cipher::{KeyIvInit, StreamCipher};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey as DhPublic, StaticSecret};

use serde::{Deserialize, Serialize};

pub const KEY_LEN: usize = 32;
pub const NONCE_LEN: usize = 12;
pub const TAG_LEN: usize = 32;
pub const SIGNATURE_LEN: usize = 64;
const INFO_PREFIX: &[u8] = b"gnomeai-messenger-v1/";

pub type Key = [u8; KEY_LEN];

pub fn random_bytes(len: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; len];
    getrandom::fill(&mut bytes).expect("operating system random source");
    bytes
}

pub fn random_key() -> Key {
    let mut key = [0u8; KEY_LEN];
    getrandom::fill(&mut key).expect("operating system random source");
    key
}

pub fn random_nonce() -> [u8; NONCE_LEN] {
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce).expect("operating system random source");
    nonce
}

pub fn random_id() -> String {
    hex::encode(random_bytes(16))
}

/// HKDF-SHA256 with the messenger domain prefix applied to `info`.
pub fn derive(ikm: &[u8], salt: &[u8], info: &str, len: usize) -> Vec<u8> {
    let mut label = Vec::with_capacity(INFO_PREFIX.len() + info.len());
    label.extend_from_slice(INFO_PREFIX);
    label.extend_from_slice(info.as_bytes());
    let hkdf = Hkdf::<Sha256>::new(Some(salt), ikm);
    let mut out = vec![0u8; len];
    hkdf.expand(&label, &mut out)
        .expect("HKDF output length is within the SHA-256 limit");
    out
}

pub fn derive_key(ikm: &[u8], salt: &[u8], info: &str) -> Key {
    let mut key = [0u8; KEY_LEN];
    key.copy_from_slice(&derive(ikm, salt, info, KEY_LEN));
    key
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(sha256(data))
}

pub fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC accepts any key");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

/// Constant-time comparison for tags and digests.
pub fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in left.iter().zip(right) {
        difference |= a ^ b;
    }
    difference == 0
}

// ---------------------------------------------------------------------------
// AEAD
// ---------------------------------------------------------------------------

fn aead_keys(key: &Key) -> (Key, Key) {
    (
        derive_key(key, &[], "aead/enc"),
        derive_key(key, &[], "aead/mac"),
    )
}

fn aead_tag(mac_key: &Key, nonce: &[u8; NONCE_LEN], aad: &[u8], ciphertext: &[u8]) -> [u8; 32] {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(mac_key).expect("HMAC accepts any key");
    mac.update(&(aad.len() as u32).to_be_bytes());
    mac.update(aad);
    mac.update(nonce);
    mac.update(ciphertext);
    mac.finalize().into_bytes().into()
}

/// `nonce || ciphertext || tag`, self-contained so stored or forwarded blobs
/// never need out-of-band framing.
pub fn seal(key: &Key, plaintext: &[u8], aad: &[u8]) -> Vec<u8> {
    let (enc_key, mac_key) = aead_keys(key);
    let nonce = random_nonce();
    let mut ciphertext = plaintext.to_vec();
    let mut cipher = chacha20::ChaCha20::new((&enc_key).into(), (&nonce).into());
    cipher.apply_keystream(&mut ciphertext);
    let tag = aead_tag(&mac_key, &nonce, aad, &ciphertext);
    let mut out = Vec::with_capacity(NONCE_LEN + ciphertext.len() + TAG_LEN);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ciphertext);
    out.extend_from_slice(&tag);
    out
}

pub fn open(key: &Key, sealed: &[u8], aad: &[u8]) -> Result<Vec<u8>> {
    if sealed.len() < NONCE_LEN + TAG_LEN {
        bail!("sealed data is truncated");
    }
    let (enc_key, mac_key) = aead_keys(key);
    let (nonce, rest) = sealed.split_at(NONCE_LEN);
    let (ciphertext, tag) = rest.split_at(rest.len() - TAG_LEN);
    let expected = aead_tag(
        &mac_key,
        nonce.try_into().expect("nonce length"),
        aad,
        ciphertext,
    );
    if !constant_time_eq(&expected, tag) {
        bail!("sealed data failed authentication");
    }
    let mut plaintext = ciphertext.to_vec();
    let mut cipher =
        chacha20::ChaCha20::new((&enc_key).into(), nonce.try_into().expect("nonce length"));
    cipher.apply_keystream(&mut plaintext);
    Ok(plaintext)
}

// ---------------------------------------------------------------------------
// Keys
// ---------------------------------------------------------------------------

/// A messenger identity. Both halves are generated from their own random seeds
/// and never touched by the device-mesh code path.
#[derive(Clone)]
pub struct IdentityKeys {
    /// Ed25519 key used to sign invites and envelopes.
    pub signing_seed: [u8; 32],
    /// X25519 key used by X3DH.
    pub dh_seed: [u8; 32],
}

impl IdentityKeys {
    pub fn generate() -> Self {
        Self {
            signing_seed: random_key(),
            dh_seed: random_key(),
        }
    }

    pub fn signing(&self) -> SigningKey {
        SigningKey::from_bytes(&self.signing_seed)
    }

    pub fn signing_public(&self) -> VerifyingKey {
        self.signing().verifying_key()
    }

    pub fn dh_secret(&self) -> StaticSecret {
        StaticSecret::from(self.dh_seed)
    }

    pub fn dh_public(&self) -> [u8; 32] {
        *DhPublic::from(&self.dh_secret()).as_bytes()
    }

    /// `user_id` is the hex of the Ed25519 identity key: it is the stable name
    /// a peer puts in an invite, and it is what signatures are checked against.
    /// The X25519 half travels separately as `identity_pk`.
    pub fn user_id(&self) -> String {
        hex::encode(self.signing_public().to_bytes())
    }
}

pub fn sign(signing: &SigningKey, data: &[u8]) -> String {
    hex::encode(signing.sign(data).to_bytes())
}

pub fn verify(signing_public: &VerifyingKey, data: &[u8], signature_hex: &str) -> Result<()> {
    let bytes =
        hex::decode(signature_hex.trim()).map_err(|_| anyhow::anyhow!("malformed signature"))?;
    let signature =
        Signature::from_slice(&bytes).map_err(|_| anyhow::anyhow!("malformed signature"))?;
    signing_public
        .verify_strict(data, &signature)
        .map_err(|_| anyhow::anyhow!("signature does not match its author"))
}

pub fn verifying_key(hex_key: &str) -> Result<VerifyingKey> {
    let bytes = hex::decode(hex_key.trim()).map_err(|_| anyhow::anyhow!("malformed public key"))?;
    let key: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("malformed public key"))?;
    VerifyingKey::from_bytes(&key).map_err(|_| anyhow::anyhow!("malformed public key"))
}

pub fn dh_public_key(hex_key: &str) -> Result<DhPublic> {
    let bytes = hex::decode(hex_key.trim()).map_err(|_| anyhow::anyhow!("malformed key"))?;
    let key: [u8; 32] = bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow::anyhow!("malformed key"))?;
    Ok(DhPublic::from(key))
}

pub fn dh(secret: &StaticSecret, public: &DhPublic) -> [u8; 32] {
    *secret.diffie_hellman(public).as_bytes()
}

// ---------------------------------------------------------------------------
// X3DH
// ---------------------------------------------------------------------------

/// What a peer needs to start a session with us. `signed_prekey_sig` proves the
/// prekey belongs to the identity that published it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PrekeyBundle {
    pub identity_pk: String,
    pub signed_prekey: String,
    pub signed_prekey_sig: String,
    #[serde(default)]
    pub one_time_prekey: Option<String>,
}

/// One side of the X3DH exchange, before the root key is split in two.
struct X3dhMaterial {
    shared: Vec<u8>,
    associated: Vec<u8>,
}

/// `DH1 = DH(IK_A, SPK_B)`, `DH2 = DH(EK_A, IK_B)`, `DH3 = DH(EK_A, SPK_B)` and,
/// when a one-time prekey is available, `DH4 = DH(EK_A, OPK_B)`.
fn x3dh_initiator(
    identity: &IdentityKeys,
    ephemeral: &StaticSecret,
    bundle: &PrekeyBundle,
) -> Result<X3dhMaterial> {
    let spk = dh_public_key(&bundle.signed_prekey)?;
    let ik = dh_public_key(&bundle.identity_pk)?;
    let mut shared = Vec::with_capacity(32 * 4);
    shared.extend_from_slice(&dh(&identity.dh_secret(), &spk));
    shared.extend_from_slice(&dh(ephemeral, &ik));
    shared.extend_from_slice(&dh(ephemeral, &spk));
    if let Some(opk) = bundle.one_time_prekey.as_deref() {
        shared.extend_from_slice(&dh(ephemeral, &dh_public_key(opk)?));
    }
    let mut associated = Vec::new();
    associated.extend_from_slice(ik.as_bytes());
    associated.extend_from_slice(DhPublic::from(&identity.dh_secret()).as_bytes());
    Ok(X3dhMaterial { shared, associated })
}

fn x3dh_responder(
    identity: &IdentityKeys,
    signed_prekey: &StaticSecret,
    one_time_prekey: Option<&StaticSecret>,
    peer_identity: &DhPublic,
    peer_ephemeral: &DhPublic,
) -> X3dhMaterial {
    let mut shared = Vec::with_capacity(32 * 4);
    let own_signed_public = DhPublic::from(signed_prekey);
    shared.extend_from_slice(&dh(signed_prekey, peer_identity));
    shared.extend_from_slice(&dh(&identity.dh_secret(), peer_ephemeral));
    shared.extend_from_slice(&dh(signed_prekey, peer_ephemeral));
    if let Some(opk) = one_time_prekey {
        shared.extend_from_slice(&dh(opk, peer_ephemeral));
    }
    let mut associated = Vec::new();
    associated.extend_from_slice(peer_identity.as_bytes());
    associated.extend_from_slice(own_signed_public.as_bytes());
    X3dhMaterial { shared, associated }
}

/// Both sides derive the same root key and the same binding transcript.
fn split(material: X3dhMaterial, peer_user_id: &str) -> (Key, Vec<u8>) {
    let root = derive_key(&material.shared, &[0u8; 32], "x3dh/root");
    let mut transcript = material.associated;
    transcript.extend_from_slice(peer_user_id.as_bytes());
    let binding = derive(&root, &[], "x3dh/binding", 32);
    transcript.extend_from_slice(&binding);
    (root, transcript)
}

/// Result of an initiator's half of X3DH.
pub struct Initiation {
    pub root: Key,
    /// Sent in the first envelope so the responder can reproduce our transcripts.
    pub ephemeral_public: String,
}

pub fn initiate(
    identity: &IdentityKeys,
    bundle: &PrekeyBundle,
    peer_user_id: &str,
) -> Result<Initiation> {
    let ephemeral = StaticSecret::from(random_key());
    let material = x3dh_initiator(identity, &ephemeral, bundle)?;
    let (root, _) = split(material, peer_user_id);
    Ok(Initiation {
        root,
        ephemeral_public: hex::encode(DhPublic::from(&ephemeral).as_bytes()),
    })
}

pub fn respond(
    identity: &IdentityKeys,
    signed_prekey: &StaticSecret,
    one_time_prekey: Option<&StaticSecret>,
    peer_identity_pk: &str,
    peer_ephemeral_pk: &str,
    peer_user_id: &str,
) -> Result<Key> {
    let peer_identity = dh_public_key(peer_identity_pk)?;
    let peer_ephemeral = dh_public_key(peer_ephemeral_pk)?;
    let material = x3dh_responder(
        identity,
        signed_prekey,
        one_time_prekey,
        &peer_identity,
        &peer_ephemeral,
    );
    Ok(split(material, peer_user_id).0)
}

// ---------------------------------------------------------------------------
// Double Ratchet
// ---------------------------------------------------------------------------

/// Wire header. It is authenticated as associated data, so a header cannot be
/// edited in transit without the message failing to open.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RatchetHeader {
    pub dh: String,
    #[serde(rename = "pn")]
    pub previous_chain_length: u32,
    #[serde(rename = "n")]
    pub message_number: u32,
}

impl RatchetHeader {
    pub fn aad(&self) -> Vec<u8> {
        format!(
            "ratchet/v1/{}:{}:{}",
            self.dh, self.previous_chain_length, self.message_number
        )
        .into_bytes()
    }
}

/// Persisted ratchet state. It is stored sealed for the account, so a stolen
/// database alone is not enough to read future messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RatchetState {
    root_key: Key,
    /// X25519 secret for our current ratchet key pair.
    local_private: [u8; 32],
    local_public: [u8; 32],
    /// Peer ratchet key, once a message has established it.
    remote_public: Option<[u8; 32]>,
    send_chain: Option<Key>,
    receive_chain: Option<Key>,
    send_count: u32,
    receive_count: u32,
    previous_send_count: u32,
    /// Skipped message keys as `(peer ratchet key, message number, key)`,
    /// bounded so a hostile header cannot exhaust memory.
    #[serde(default)]
    skipped: Vec<([u8; 32], u32, Key)>,
}

const MAX_SKIPPED: usize = 1024;

/// `KDF_RK`: the root chain step. Both directions derive the same pair because
/// the initiator labels its sending chain exactly where the responder labels
/// its receiving chain.
fn kdf_root(root: &Key, shared: &[u8; 32]) -> (Key, Key) {
    let next_root = derive_key(root, shared, "ratchet/root");
    let chain = derive_key(&next_root, &[], "ratchet/chain");
    (next_root, chain)
}

impl RatchetState {
    /// Initiator side. `remote_public` is the peer's signed prekey, so the first
    /// sending chain exists before any reply.
    pub fn initiator(root: Key, remote_public: [u8; 32]) -> Self {
        let local = StaticSecret::from(random_key());
        let (root_key, send_chain) = kdf_root(&root, &dh(&local, &DhPublic::from(remote_public)));
        Self {
            root_key,
            local_private: local.to_bytes(),
            local_public: *DhPublic::from(&local).as_bytes(),
            remote_public: Some(remote_public),
            send_chain: Some(send_chain),
            receive_chain: None,
            send_count: 0,
            receive_count: 0,
            previous_send_count: 0,
            skipped: Vec::new(),
        }
    }

    /// Responder side. The local key is the signed prekey the invite published,
    /// so `DH(signed_prekey, peer_ratchet_key)` reproduces the initiator's
    /// first chain exactly.
    pub fn responder(root: Key, signed_prekey: &StaticSecret) -> Self {
        Self {
            root_key: root,
            local_private: signed_prekey.to_bytes(),
            local_public: *DhPublic::from(signed_prekey).as_bytes(),
            remote_public: None,
            send_chain: None,
            receive_chain: None,
            send_count: 0,
            receive_count: 0,
            previous_send_count: 0,
            skipped: Vec::new(),
        }
    }

    fn next_send_key(&mut self) -> Key {
        let chain = self
            .send_chain
            .expect("sending requires an established chain");
        let message_key = derive_key(&chain, &[], "ratchet/message");
        self.send_chain = Some(derive_key(&chain, &[], "ratchet/next"));
        self.send_count += 1;
        message_key
    }

    fn next_receive_key(&mut self) -> Result<Key> {
        let chain = self
            .receive_chain
            .ok_or_else(|| anyhow::anyhow!("no receiving chain has been established yet"))?;
        let message_key = derive_key(&chain, &[], "ratchet/message");
        self.receive_chain = Some(derive_key(&chain, &[], "ratchet/next"));
        self.receive_count += 1;
        Ok(message_key)
    }

    pub fn encrypt(&mut self, plaintext: &[u8]) -> (RatchetHeader, Vec<u8>) {
        let header = RatchetHeader {
            dh: hex::encode(self.local_public),
            previous_chain_length: self.previous_send_count,
            message_number: self.send_count,
        };
        let key = self.next_send_key();
        let body = seal(&key, plaintext, &header.aad());
        (header, body)
    }

    fn skipped_key(&mut self, remote: &[u8; 32], number: u32) -> Option<Key> {
        let index = self
            .skipped
            .iter()
            .position(|(candidate, skipped, _)| candidate == remote && *skipped == number)?;
        Some(self.skipped.remove(index).2)
    }

    fn remember_skipped(&mut self, remote: [u8; 32], number: u32, key: Key) {
        if self.skipped.len() >= MAX_SKIPPED {
            self.skipped.remove(0);
        }
        self.skipped.push((remote, number, key));
    }

    /// Keep the keys of messages that never arrived, so a later delivery in a
    /// different order still opens.
    fn skip_receive_keys(&mut self, until: u32) -> Result<()> {
        let remote = self.remote_public.ok_or_else(|| {
            anyhow::anyhow!("a receiving chain cannot be skipped before a ratchet key")
        })?;
        while self.receive_count < until {
            let number = self.receive_count;
            let key = self.next_receive_key()?;
            self.remember_skipped(remote, number, key);
        }
        Ok(())
    }

    /// One DH ratchet step: finish the current receiving chain, derive the new
    /// one, then rotate our key and derive the next sending chain.
    fn ratchet_step(&mut self, remote_public: [u8; 32]) {
        self.previous_send_count = self.send_count;
        self.send_count = 0;
        self.remote_public = Some(remote_public);

        let local = StaticSecret::from(self.local_private);
        let (root_key, receive_chain) =
            kdf_root(&self.root_key, &dh(&local, &DhPublic::from(remote_public)));
        self.root_key = root_key;
        self.receive_chain = Some(receive_chain);
        self.receive_count = 0;

        let next = StaticSecret::from(random_key());
        self.local_private = next.to_bytes();
        self.local_public = *DhPublic::from(&next).as_bytes();
        let (root_key, send_chain) =
            kdf_root(&self.root_key, &dh(&next, &DhPublic::from(remote_public)));
        self.root_key = root_key;
        self.send_chain = Some(send_chain);
    }

    pub fn decrypt(&mut self, header: &RatchetHeader, body: &[u8]) -> Result<Vec<u8>> {
        let remote = *dh_public_key(&header.dh)?.as_bytes();
        if let Some(key) = self.skipped_key(&remote, header.message_number) {
            return open(&key, body, &header.aad());
        }
        if self.remote_public != Some(remote) {
            // A responder has no receiving chain before its first header, so
            // there is nothing to skip yet.
            if self.receive_chain.is_some() {
                self.skip_receive_keys(header.previous_chain_length)?;
            }
            self.ratchet_step(remote);
        }
        if header.message_number < self.receive_count {
            bail!("a message was replayed or arrived out of order without its key");
        }
        self.skip_receive_keys(header.message_number)?;
        let key = self.next_receive_key()?;
        open(&key, body, &header.aad())
    }

    /// Public ratchet key, useful for diagnostics and for the UI's safety view.
    pub fn public_key(&self) -> String {
        hex::encode(self.local_public)
    }
}

// ---------------------------------------------------------------------------
// Sender-key groups
// ---------------------------------------------------------------------------

/// One group member's sending chain. Every recipient keeps one of these per
/// (group, sender) pair, so group traffic never needs a central server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SenderChain {
    chain_key: Key,
    iteration: u32,
}

impl SenderChain {
    pub fn new(chain_key: Key) -> Self {
        Self {
            chain_key,
            iteration: 0,
        }
    }

    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("malformed sender chain"))
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("a sender chain always serialises")
    }

    pub fn iteration(&self) -> u32 {
        self.iteration
    }

    /// Consume one step of the sending chain, returning its index and key.
    /// Named `advance`, not `next`, so it can never be mistaken for
    /// `Iterator::next` at a call site.
    pub fn advance(&mut self) -> (u32, Key) {
        let index = self.iteration;
        let message_key = derive_key(&self.chain_key, &[], "sender-key/message");
        self.chain_key = derive_key(&self.chain_key, &[], "sender-key/next");
        self.iteration += 1;
        (index, message_key)
    }

    /// Skip forward to `iteration`, so out-of-order group messages still open.
    pub fn advance_to(&mut self, iteration: u32) -> Result<Key> {
        if iteration < self.iteration {
            bail!("a group message arrived with an older chain index");
        }
        while self.iteration < iteration {
            let _ = self.advance();
        }
        Ok(self.advance().1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle(
        identity: &IdentityKeys,
        signed_prekey: &StaticSecret,
        opk: Option<&StaticSecret>,
        one_time: bool,
    ) -> PrekeyBundle {
        let spk_public = DhPublic::from(signed_prekey);
        let signature = sign(
            &identity.signing(),
            &[
                b"gnomeai-messenger-signed-prekey-v1".as_slice(),
                spk_public.as_bytes(),
            ]
            .concat(),
        );
        PrekeyBundle {
            identity_pk: hex::encode(identity.dh_public()),
            signed_prekey: hex::encode(spk_public.as_bytes()),
            signed_prekey_sig: signature,
            one_time_prekey: if one_time {
                opk.map(|k| hex::encode(DhPublic::from(k).as_bytes()))
            } else {
                None
            },
        }
    }

    #[test]
    fn aead_round_trips_and_rejects_tampering() {
        let key = random_key();
        let sealed = seal(&key, b"hello messenger", b"aad");
        assert_eq!(open(&key, &sealed, b"aad").unwrap(), b"hello messenger");
        assert!(open(&key, &sealed, b"other").is_err());
        let mut tampered = sealed.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 1;
        assert!(open(&key, &tampered, b"aad").is_err());
    }

    #[test]
    fn x3dh_both_sides_agree_with_and_without_one_time_prekey() {
        for with_opk in [false, true] {
            let alice = IdentityKeys::generate();
            let bob = IdentityKeys::generate();
            let bob_signed = StaticSecret::from(random_key());
            let bob_opk = StaticSecret::from(random_key());
            let bob_public = DhPublic::from(&bob_signed).as_bytes().to_vec();
            let offered = bundle(&bob, &bob_signed, Some(&bob_opk), with_opk);

            let initiation = initiate(&alice, &offered, &bob.user_id()).unwrap();
            let responder_root = respond(
                &bob,
                &bob_signed,
                if with_opk { Some(&bob_opk) } else { None },
                &hex::encode(alice.dh_public()),
                &initiation.ephemeral_public,
                &alice.user_id(),
            )
            .unwrap();
            assert_eq!(initiation.root, responder_root, "opk={with_opk}");
            assert_eq!(bob_public.len(), 32);
        }
    }

    #[test]
    fn signed_prekey_signature_is_checkable() {
        let identity = IdentityKeys::generate();
        let signed = StaticSecret::from(random_key());
        let offered = bundle(&identity, &signed, None, false);
        let spk = hex::decode(&offered.signed_prekey).unwrap();
        let payload = [b"gnomeai-messenger-signed-prekey-v1".as_slice(), &spk].concat();
        verify(
            &identity.signing_public(),
            &payload,
            &offered.signed_prekey_sig,
        )
        .unwrap();
        let other = IdentityKeys::generate();
        assert!(
            verify(
                &other.signing_public(),
                &payload,
                &offered.signed_prekey_sig
            )
            .is_err()
        );
    }

    /// Two paired devices can open the conversation in the same instant.
    ///
    /// Once each side holds the other's invite, both composers are enabled, so
    /// both may seal a first message before either has read one. Each seals
    /// under its *own* X3DH root, and the two roots differ. A single session per
    /// peer therefore cannot serve both streams:
    /// whichever side answers the peer's direction would answer under a root
    /// that peer never derived, and the receiver reports exactly
    /// "sealed data failed authentication".
    ///
    /// So the state keeps one ratchet per direction. This test pins the
    /// property the fix relies on: the responder half of the peer's initiation
    /// opens their first message even when we started our own direction too.
    #[test]
    fn a_crossed_initiation_opens_on_both_directions() {
        let alice_id = IdentityKeys::generate();
        let bob_id = IdentityKeys::generate();
        let alice_signed = StaticSecret::from(random_key());
        let bob_signed = StaticSecret::from(random_key());

        // Each side scanned the other's invite, so each can initiate.
        let alice_view = bundle(&bob_id, &bob_signed, None, false);
        let bob_view = bundle(&alice_id, &alice_signed, None, false);
        let alice_init = initiate(&alice_id, &alice_view, &bob_id.user_id()).unwrap();
        let bob_init = initiate(&bob_id, &bob_view, &alice_id.user_id()).unwrap();

        // Sending halves: the direction each side opened.
        let mut alice_out = RatchetState::initiator(
            alice_init.root,
            dh_public_key(&alice_view.signed_prekey).unwrap().to_bytes(),
        );
        let mut bob_out = RatchetState::initiator(
            bob_init.root,
            dh_public_key(&bob_view.signed_prekey).unwrap().to_bytes(),
        );
        // Receiving halves: the direction the peer opened.
        let alice_in_root = respond(
            &alice_id,
            &alice_signed,
            None,
            &hex::encode(bob_id.dh_public()),
            &bob_init.ephemeral_public,
            &bob_id.user_id(),
        )
        .unwrap();
        let mut alice_in = RatchetState::responder(alice_in_root, &alice_signed);
        let bob_in_root = respond(
            &bob_id,
            &bob_signed,
            None,
            &hex::encode(alice_id.dh_public()),
            &alice_init.ephemeral_public,
            &alice_id.user_id(),
        )
        .unwrap();
        let mut bob_in = RatchetState::responder(bob_in_root, &bob_signed);

        // Both write before either has read a single message.
        let (alice_header, alice_body) = alice_out.encrypt(b"hi from alice");
        let (bob_header, bob_body) = bob_out.encrypt(b"hi from bob");

        // Neither direction is lost to the collision.
        assert_eq!(
            bob_in.decrypt(&alice_header, &alice_body).unwrap(),
            b"hi from alice"
        );
        assert_eq!(
            alice_in.decrypt(&bob_header, &bob_body).unwrap(),
            b"hi from bob"
        );
    }

    #[test]
    fn double_ratchet_conversation_survives_reordering() {
        let alice_id = IdentityKeys::generate();
        let bob_id = IdentityKeys::generate();
        let bob_signed = StaticSecret::from(random_key());
        let offered = bundle(&bob_id, &bob_signed, None, false);
        let initiation = initiate(&alice_id, &offered, &bob_id.user_id()).unwrap();
        let mut alice = RatchetState::initiator(
            initiation.root,
            dh_public_key(&offered.signed_prekey).unwrap().to_bytes(),
        );
        let bob_root = respond(
            &bob_id,
            &bob_signed,
            None,
            &hex::encode(alice_id.dh_public()),
            &initiation.ephemeral_public,
            &alice_id.user_id(),
        )
        .unwrap();
        let mut bob = RatchetState::responder(bob_root, &bob_signed);

        let (h1, c1) = alice.encrypt(b"first");
        let (h2, c2) = alice.encrypt(b"second");
        let (h3, c3) = alice.encrypt(b"third");
        // Deliberately out of order, with one message held back.
        assert_eq!(bob.decrypt(&h2, &c2).unwrap(), b"second");
        assert_eq!(bob.decrypt(&h1, &c1).unwrap(), b"first");

        let (b1, bc1) = bob.encrypt(b"reply");
        assert_eq!(alice.decrypt(&b1, &bc1).unwrap(), b"reply");

        assert_eq!(bob.decrypt(&h3, &c3).unwrap(), b"third");
        let (a2, ac2) = alice.encrypt(b"after a step");
        assert_eq!(bob.decrypt(&a2, &ac2).unwrap(), b"after a step");
        assert_ne!(alice.public_key(), bob.public_key());
    }

    #[test]
    fn ratchet_rejects_a_modified_header() {
        let ids = IdentityKeys::generate();
        let signed = StaticSecret::from(random_key());
        let offered = bundle(&ids, &signed, None, false);
        let initiation = initiate(&ids, &offered, &ids.user_id()).unwrap();
        let mut state = RatchetState::initiator(
            initiation.root,
            dh_public_key(&offered.signed_prekey).unwrap().to_bytes(),
        );
        let (header, body) = state.encrypt(b"payload");
        let mut forged = header.clone();
        forged.message_number += 5;
        assert!(state.decrypt(&forged, &body).is_err());
    }

    #[test]
    fn sender_chain_is_forward_only() {
        let mut chain = SenderChain::new(random_key());
        let (i0, k0) = chain.advance();
        let (_i1, _k1) = chain.advance();
        assert_eq!(i0, 0);
        assert_ne!(k0, derive_key(&k0, &[], "sender-key/next"));
        assert!(chain.advance_to(0).is_err());
    }

    #[test]
    fn ratchet_state_round_trips_through_storage() {
        let root = random_key();
        let signed = StaticSecret::from(random_key());
        let mut state = RatchetState::responder(root, &signed);
        let bytes = serde_json::to_vec(&state).unwrap();
        let mut restored: RatchetState = serde_json::from_slice(&bytes).unwrap();
        // A restored responder still opens an initiator's first envelope.
        let mut initiator = RatchetState::initiator(root, *DhPublic::from(&signed).as_bytes());
        let (header, body) = initiator.encrypt(b"persisted");
        assert_eq!(restored.decrypt(&header, &body).unwrap(), b"persisted");
        state = restored;
        assert!(!state.public_key().is_empty());
    }
}
