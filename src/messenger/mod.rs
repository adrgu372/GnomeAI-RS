//! GnomeAI Messenger: end-to-end encrypted P2P chat over Tor/Arti.
//!
//! The messenger owns its own identity, its own onion service per account and
//! its own tables. It never reads a device-mesh key and never shares an onion
//! with the mesh: that separation is what keeps "my phone and my PC" from
//! becoming "anyone who paired my phone can read my chat".
//!
//! Layers, bottom to top:
//!
//! * `crypto` — AEAD, X3DH, Double Ratchet, sender keys.
//! * `media` — chunked attachment sealing and hashing.
//! * `store` — SQLite tables for accounts, contacts, conversations, messages.
//! * `wire` — the small JSON envelopes that travel on a Tor stream.
//! * `peer` — the onion listener plus the outbound sender fan-out.
//! * `service` — the Op/Event bridge used by the core and the desktop UI.

pub mod crypto;
pub mod media;
pub mod peer;
pub mod service;
pub mod store;
pub mod wire;

pub use service::MessengerService;
pub use store::{MessageRecord, MessageStatus, MessengerStore};
pub use wire::{MessageKind, OutgoingAttachment};
