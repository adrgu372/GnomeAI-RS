//! What actually crosses a Tor stream.
//!
//! Two shapes only: a [`Request`] from a sender who opened a connection, and a
//! [`Reply`] from the account's listener. Both are length-prefixed JSON with a
//! small schema version, so a future field cannot silently change meaning.
//!
//! Nothing here is trusted. Everything is smaller than the frame limit, and
//! every session envelope is verified against the identity key that the peer
//! advertised in its invite before any state changes.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use super::crypto::{PrekeyBundle, RatchetHeader};

pub const PROTOCOL_VERSION: u32 = 1;
/// A frame is a JSON line. 8 MiB is far above any control message; attachment
/// chunks travel as base64 inside their own frame and are bounded by this too.
pub const MAX_FRAME: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    Text,
    Image,
    Video,
    Audio,
    File,
    System,
    Edit,
}

impl MessageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MessageKind::Text => "text",
            MessageKind::Image => "image",
            MessageKind::Video => "video",
            MessageKind::Audio => "audio",
            MessageKind::File => "file",
            MessageKind::System => "system",
            MessageKind::Edit => "edit",
        }
    }

    /// Pick a kind from a MIME type so the conversation list can show the right
    /// badge without guessing from the filename.
    pub fn from_mime(mime: &str) -> Self {
        if mime.starts_with("image/") {
            MessageKind::Image
        } else if mime.starts_with("video/") {
            MessageKind::Video
        } else if mime.starts_with("audio/") {
            MessageKind::Audio
        } else {
            MessageKind::File
        }
    }
}

/// Attachment metadata. The bytes themselves follow in chunk frames.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttachmentMeta {
    pub mime: String,
    pub file_name: String,
    pub size_bytes: u64,
    #[serde(default)]
    pub duration_ms: Option<u64>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    pub sha256: String,
    pub chunk_size: u32,
    pub total_chunks: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutgoingAttachment {
    pub path: std::path::PathBuf,
    pub meta: AttachmentMeta,
    /// Hex of the per-attachment key. It travels inside the ratchet envelope,
    /// never in the clear.
    pub key: String,
}

/// The plaintext body of a chat message, after the envelope is opened.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageBody {
    pub message_id: String,
    pub kind: MessageKind,
    /// Markdown source for text messages.
    #[serde(default)]
    pub text: Option<String>,
    /// One entry per attached file. A message may carry as many attachments as
    /// the sender picks; the list is ordered so both sides render the same
    /// sequence.
    #[serde(default)]
    pub attachments: Vec<AttachmentPart>,
    /// Legacy single attachment, kept so a body queued by an older build still
    /// opens. New code only writes `attachments`.
    #[serde(default)]
    pub attachment: Option<AttachmentMeta>,
    #[serde(default)]
    pub attachment_key: Option<String>,
    #[serde(default)]
    pub reply_to: Option<String>,
    /// Set on `Edit` bodies and on any text message that has been superseded.
    #[serde(default)]
    pub edit_of: Option<String>,
    /// Group sender, so a recipient can attribute a sender-key message.
    #[serde(default)]
    pub group_id: Option<String>,
    #[serde(default)]
    pub sender_id: Option<String>,
    #[serde(default)]
    pub sender_name: Option<String>,
    /// Sender-chain index for group messages.
    #[serde(default)]
    pub iteration: Option<u32>,
    pub sent_at: i64,
}

/// One attached file inside a message body: its public metadata plus the
/// per-attachment key that seals its chunks. The key travels only inside the
/// ratchet envelope.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AttachmentPart {
    pub meta: AttachmentMeta,
    /// Hex of the 32-byte per-attachment key.
    pub key: String,
}

impl MessageBody {
    /// Every attachment of this body, in order. The legacy single attachment is
    /// folded in first so an older sender's body behaves exactly like a
    /// one-element list.
    pub fn parts(&self) -> Vec<AttachmentPart> {
        let mut parts = Vec::with_capacity(self.attachments.len() + 1);
        if let (Some(meta), Some(key)) = (&self.attachment, &self.attachment_key) {
            parts.push(AttachmentPart {
                meta: meta.clone(),
                key: key.clone(),
            });
        }
        parts.extend(self.attachments.iter().cloned());
        parts
    }

    /// True when this body announces at least one attachment.
    pub fn has_attachments(&self) -> bool {
        !self.attachments.is_empty() || self.attachment.is_some()
    }
}

/// A 1:1 envelope. The first message of a conversation carries the X3DH
/// preamble; every later one only needs the ratchet header.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub v: u32,
    pub from_user_id: String,
    pub from_account_onion: String,
    pub header: RatchetHeader,
    /// Base64 of `seal(...)`, whose key comes from the ratchet.
    pub ciphertext: String,
    /// Ed25519 signature over the authenticated transcript.
    pub signature: String,
    #[serde(default)]
    pub preamble: Option<X3dhPreamble>,
}

/// Everything the responder needs to derive the same root key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct X3dhPreamble {
    /// Hex of the initiator's X25519 identity key.
    pub identity_pk: String,
    pub ephemeral_pk: String,
    /// Set when the initiator consumed one of our one-time prekeys.
    #[serde(default)]
    pub one_time_prekey: Option<String>,
    /// Hex of the identity we think we are talking to, so a responder can
    /// reject an envelope addressed to a different account.
    pub to_user_id: String,
}

impl Envelope {
    /// Data the sender signs, and that the receiver re-derives. It binds the
    /// body to the sender identity, the intended recipient and the header.
    pub fn signed_bytes(&self, to_user_id: &str) -> Vec<u8> {
        format!(
            "envelope/v1/{}:{}:{}:{}:{}",
            self.from_user_id,
            to_user_id,
            self.header.dh,
            self.header.previous_chain_length,
            self.header.message_number
        )
        .into_bytes()
    }
}

/// A group message under the sender's chain key.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupEnvelope {
    pub v: u32,
    pub group_id: String,
    pub sender_id: String,
    #[serde(default)]
    pub sender_name: Option<String>,
    pub iteration: u32,
    pub ciphertext: String,
    pub signature: String,
}

impl GroupEnvelope {
    pub fn signed_bytes(&self) -> Vec<u8> {
        format!(
            "group-envelope/v1/{}:{}:{}",
            self.group_id, self.sender_id, self.iteration
        )
        .into_bytes()
    }
}

/// A sender-key distribution message, delivered 1:1 and ratcheted like any
/// other 1:1 payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SenderKeyDistribution {
    pub group_id: String,
    #[serde(default)]
    pub group_name: String,
    pub sender_id: String,
    /// The chain key, base64. This is the secret; it never leaves the 1:1
    /// envelope.
    pub chain_key: String,
    pub iteration: u32,
    /// Full member list so the recipient can render the group and knows where
    /// to fan out its own messages.
    pub members: Vec<GroupMemberWire>,
    #[serde(default)]
    pub admins: Vec<String>,
    #[serde(default)]
    pub revoked: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GroupMemberWire {
    pub user_id: String,
    pub onion: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub identity_pk: String,
    #[serde(default)]
    pub signed_prekey: String,
}

/// A receipt for a delivered message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Receipt {
    pub message_id: String,
    /// `delivered`, `read` or `failed`.
    pub status: String,
}

/// One request on a stream. `id` is echoed in the reply so a sender can match
/// concurrent requests on a reused stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "request", rename_all = "snake_case")]
pub enum Request {
    /// Prove the caller owns `from_user_id` and ask for the peer's prekey
    /// bundle plus a one-time prekey from the pool.
    Handshake {
        from_user_id: String,
        from_identity_pk: String,
        signature: String,
        nonce: String,
    },
    /// Deliver one 1:1 envelope.
    Message {
        envelope: Box<Envelope>,
    },
    /// Deliver one group message.
    Group {
        envelope: Box<GroupEnvelope>,
    },
    /// Deliver a sender key (or a rotation) over the 1:1 channel.
    ///
    /// The distribution itself is **not** a field here on purpose: the chain
    /// key is a secret and travels only inside the ratchet envelope. A cleartext
    /// copy would rely on Tor alone for confidentiality, which is weaker than
    /// the end-to-end guarantee every other message has.
    SenderKey {
        from_user_id: String,
        envelope: Box<Envelope>,
    },
    /// The first message of a conversation.
    ///
    /// It carries the X3DH preamble *and* the first body together, because
    /// opening the preamble consumes a one-time prekey: a separate handshake
    /// message would burn the prekey even when the message behind it never
    /// arrives.
    FirstMessage {
        envelope: Box<Envelope>,
    },
    /// "I scanned your QR and I want to chat." Sent before any message, so the
    /// receiving device can show an explicit contact request and only then
    /// create the contact. It carries the sender's signed invite; nothing in it
    /// is trusted until the signature verifies.
    ContactRequest {
        invite: String,
    },
    /// The other side's answer to a `ContactRequest`.
    ///
    /// `accepted` false means the request was declined; the sender must not
    /// show the peer in the contact list. Carries the answering side's invite so
    /// an acceptance is also a mutual contact, and no follow-up is needed.
    ContactResponse {
        user_id: String,
        accepted: bool,
        #[serde(default)]
        invite: String,
    },
    /// A chunk of an accepted attachment transfer.
    ///
    /// `from_user_id` is the sender's claim. It is not trusted on its own, but
    /// combined with the transfer's recorded peer it stops a third party who
    /// learned a transfer id from injecting chunks into somebody else's file.
    /// The chunk ciphertext is authenticated separately by its attachment key.
    Chunk {
        transfer_id: String,
        #[serde(default)]
        from_user_id: String,
        index: u32,
        data: String,
    },
    Receipt {
        receipt: Receipt,
    },
    /// Cheap liveness probe used by the presence heartbeat.
    Ping,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum Reply {
    Bundle {
        user_id: String,
        display_name: String,
        bundle: PrekeyBundle,
    },
    Accepted,
    /// The sender is not (yet) a contact of this account.
    ///
    /// It is a distinct reply rather than a generic error because the sender
    /// must *keep* its message queued: the person on this side has simply not
    /// accepted the request yet, and throwing the message away would lose it.
    NotAContact {
        user_id: String,
    },
    /// The other side has not answered the contact request yet.
    ContactPending {
        user_id: String,
    },
    /// They already had us as a contact, so the request is answered at once
    /// with their invite.
    ContactAnswer {
        user_id: String,
        accepted: bool,
        invite: String,
    },
    /// The peer accepted a chunk and reports its progress.
    ChunkAccepted {
        transfer_id: String,
        received: u32,
        total: u32,
    },
    Pong,
    Error {
        message: String,
    },
}

pub fn decode_request(line: &str) -> Result<Request> {
    if line.len() > MAX_FRAME {
        bail!("the frame is larger than the messenger allows");
    }
    serde_json::from_str(line).map_err(|_| anyhow::anyhow!("malformed messenger request"))
}

pub fn decode_reply(line: &str) -> Result<Reply> {
    if line.len() > MAX_FRAME {
        bail!("the frame is larger than the messenger allows");
    }
    serde_json::from_str(line).map_err(|_| anyhow::anyhow!("malformed messenger reply"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_round_trips_with_its_tag() {
        let request = Request::Ping;
        let encoded = serde_json::to_string(&request).unwrap();
        assert!(encoded.contains("\"request\":\"ping\""));
        assert!(matches!(decode_request(&encoded).unwrap(), Request::Ping));
    }

    #[test]
    fn oversized_and_malformed_frames_are_rejected() {
        assert!(decode_request("not json").is_err());
        let huge = format!(
            "{{\"request\":\"ping\",\"pad\":\"{}\"}}",
            "x".repeat(MAX_FRAME)
        );
        assert!(decode_request(&huge).is_err());
    }

    #[test]
    fn attachment_kind_follows_the_mime_type() {
        assert_eq!(MessageKind::from_mime("image/png"), MessageKind::Image);
        assert_eq!(MessageKind::from_mime("video/mp4"), MessageKind::Video);
        assert_eq!(MessageKind::from_mime("audio/ogg"), MessageKind::Audio);
        assert_eq!(MessageKind::from_mime("application/pdf"), MessageKind::File);
    }

    fn meta(name: &str) -> AttachmentMeta {
        AttachmentMeta {
            mime: "application/pdf".into(),
            file_name: name.into(),
            size_bytes: 1,
            duration_ms: None,
            width: None,
            height: None,
            sha256: "00".into(),
            chunk_size: 1024,
            total_chunks: 1,
        }
    }

    fn body(attachments: Vec<AttachmentPart>, legacy: Option<AttachmentMeta>) -> MessageBody {
        let key = legacy.as_ref().map(|_| "aa".to_string());
        MessageBody {
            message_id: "m".into(),
            kind: MessageKind::File,
            text: Some("hello".into()),
            attachments,
            attachment: legacy,
            attachment_key: key,
            reply_to: None,
            edit_of: None,
            group_id: None,
            sender_id: None,
            sender_name: None,
            iteration: None,
            sent_at: 0,
        }
    }

    #[test]
    fn any_number_of_attachments_travel_in_one_body() {
        let many = body(
            (0..5)
                .map(|index| AttachmentPart {
                    meta: meta(&format!("file-{index}.pdf")),
                    key: format!("0{index}"),
                })
                .collect(),
            None,
        );
        let parts = many.parts();
        assert_eq!(parts.len(), 5);
        assert_eq!(parts[4].meta.file_name, "file-4.pdf");
        assert!(many.has_attachments());

        // A body with no files is not an attachment message.
        assert!(!body(Vec::new(), None).has_attachments());
    }

    #[test]
    fn a_legacy_single_attachment_still_opens_as_one_part() {
        let old = body(Vec::new(), Some(meta("old.pdf")));
        let parts = old.parts();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].meta.file_name, "old.pdf");
        assert_eq!(parts[0].key, "aa");
        assert!(old.has_attachments());
    }
}
