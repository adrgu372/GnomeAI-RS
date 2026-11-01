//! The messenger as the core sees it: one object that answers `Op`s and emits
//! `Event`s, exactly like every other subsystem.
//!
//! Delivery model, stated plainly because it decides the whole design:
//!
//! * There is no server and no store-and-forward anywhere on the network.
//! * A message that cannot be delivered right now is written `Pending` and
//!   sealed into `messenger_outbox`. When the peer's onion answers again, the
//!   queue is flushed in order.
//! * A group message is sealed once by its sender and then the same ciphertext
//!   goes to each member over that member's own onion (sender fan-out). The UI
//!   shows "delivered to X/Y".
//!
//! Everything that touches Tor runs on a spawned task. The core loop must never
//! wait for a circuit: a 90-second Tor timeout would freeze the agent.

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::{mpsc, watch};

use crate::protocol::{Event, Op};

use super::{
    crypto::{self, PrekeyBundle, RatchetState, SenderChain},
    media,
    peer::{self, MeshHandle},
    store::{
        ConversationSummary, GroupRecord, MAX_GROUP_MEMBERS, MessageRecord, MessageStatus,
        MessengerAccount, MessengerContact, MessengerStore, NewMessage, OutboxItem,
    },
    wire::{
        AttachmentMeta, AttachmentPart, Envelope, GroupEnvelope, GroupMemberWire, MessageBody,
        MessageKind, PROTOCOL_VERSION, Receipt, Reply, Request, SenderKeyDistribution,
        X3dhPreamble,
    },
};

/// How long one peer gets to answer. Tor circuits are slow; anything shorter
/// turns a slow relay into a false "offline".
const DELIVERY_TIMEOUT: Duration = Duration::from_secs(90);
const PRESENCE_TIMEOUT: Duration = Duration::from_secs(25);
/// How often a contact is pinged.
///
/// This tick is also what flushes the outbox after a peer comes back and what
/// re-sends an unanswered request, so it is the floor on how long a person
/// waits. 45 s was tuned to spare battery on a phone that stays connected; in
/// practice it is the delay someone sees when they unlock the other device and
/// nothing arrives. A ping is one small frame, so the shorter tick is the better
/// trade.
const PRESENCE_INTERVAL: Duration = Duration::from_secs(15);
/// A retried contact request runs inside the presence tick, which must stay
/// responsive for the pings that follow it. The first attempt keeps the full
/// delivery budget; a retry is already known to be re-asking an unanswered
/// request, so a shorter one is the right trade.
const REQUEST_RETRY_TIMEOUT: Duration = Duration::from_secs(45);
const ONION_BUDGET: Duration = Duration::from_secs(240);
/// How long a queue waits after a peer turned out to be offline. Long enough
/// that an absent peer costs no circuits, short enough that a returning peer is
/// noticed quickly by a user watching the transcript.
const RETRY_OFFLINE: Duration = Duration::from_secs(20);
/// How long a pass waits before re-checking the queue after Tor's SOCKS port
/// was not ready yet. Same reasoning as `RETRY_OFFLINE`: no spinning, but the
/// queue always has a next attempt.
const RETRY_TOR_NOT_READY: Duration = Duration::from_secs(30);
/// Enough prekeys for a burst of new conversations, cheap to regenerate.
const ONE_TIME_PREKEY_TARGET: usize = 20;

// ---------------------------------------------------------------------------
// Views sent to the UI
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountView {
    pub account_id: String,
    pub user_id: String,
    pub identity_pk: String,
    pub identity_public: String,
    pub signed_prekey: String,
    pub prekey_signature: String,
    pub display_name: String,
    pub onion_address: String,
    pub created_at: i64,
    pub active: bool,
    pub pro: bool,
    pub chunk_size: u32,
    pub online: bool,
}

impl AccountView {
    fn new(account: &MessengerAccount, online: bool) -> Self {
        Self {
            account_id: account.account_id.clone(),
            user_id: account.user_id.clone(),
            identity_pk: account.identity_pk.clone(),
            identity_public: account.identity_public.clone(),
            signed_prekey: account.signed_prekey.clone(),
            prekey_signature: account.prekey_signature.clone(),
            display_name: account.display_name.clone(),
            onion_address: account.onion_address.clone(),
            created_at: account.created_at,
            active: account.active,
            pro: account.pro,
            chunk_size: account.chunk_size,
            online,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageView {
    pub id: String,
    pub conversation_id: String,
    pub direction: String,
    pub from_user_id: String,
    pub to_user_id: String,
    pub ts: i64,
    pub kind: String,
    pub text: Option<String>,
    pub reply_to: Option<String>,
    pub edited: bool,
    pub edit_ts: Option<i64>,
    pub media: Option<serde_json::Value>,
    pub status: String,
    pub delivered_to: u32,
    pub total_members: u32,
}

impl MessageView {
    fn from_record(record: &MessageRecord) -> Self {
        Self {
            id: record.id.clone(),
            conversation_id: record.conversation_id.clone(),
            direction: record.direction.clone(),
            from_user_id: record.from_user_id.clone(),
            to_user_id: record.to_user_id.clone(),
            ts: record.ts,
            kind: record.kind.clone(),
            text: record.text.clone(),
            reply_to: record.reply_to.clone(),
            edited: record.edited,
            edit_ts: record.edit_ts,
            media: record
                .media_json
                .as_deref()
                .and_then(|encoded| serde_json::from_str(encoded).ok()),
            status: record.status.as_str().to_string(),
            delivered_to: record.delivered_to,
            total_members: record.total_members,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationView {
    pub conversation_id: String,
    pub kind: String,
    pub target: String,
    pub title: String,
    pub subtitle: String,
    pub unread: i64,
    pub updated_at: i64,
    pub last_kind: String,
    pub preview: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupView {
    pub group_id: String,
    pub name: String,
    pub created_by: String,
    pub created_at: i64,
    pub members: Vec<GroupMemberWire>,
    pub admins: Vec<String>,
    pub member_limit: usize,
}

impl GroupView {
    fn from_record(group: &GroupRecord) -> Self {
        Self {
            group_id: group.group_id.clone(),
            name: group.name.clone(),
            created_by: group.created_by.clone(),
            created_at: group.created_at,
            members: group.members.clone(),
            admins: group.admins.clone(),
            member_limit: MAX_GROUP_MEMBERS,
        }
    }
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

pub struct MessengerService {
    inner: Arc<Inner>,
}

struct Inner {
    store: MessengerStore,
    mesh: Arc<dyn MeshHandle>,
    events: mpsc::Sender<Event>,
    /// One listener per account, keyed by account id.
    listeners: Mutex<HashMap<String, Listener>>,
    /// Attachment transfers currently assembling on this device.
    transfers: Mutex<HashMap<String, IncomingTransfer>>,
    /// Which peers answered a ping, so presence is reported on transitions.
    presence: Mutex<HashSet<(String, String)>>,
    /// One delivery worker at a time per account, so a burst of sends does not
    /// open a hundred circuits.
    delivering: Mutex<HashSet<String>>,
    /// Accounts that had a send arrive while their delivery pass was running.
    reschedule: Mutex<HashSet<String>>,
}

struct Listener {
    stop: watch::Sender<bool>,
    online: Arc<AtomicBool>,
}

struct IncomingTransfer {
    account_id: String,
    peer_user_id: String,
    message_id: String,
    meta: AttachmentMeta,
    incoming: media::Incoming,
}

impl MessengerService {
    pub fn new(
        store: MessengerStore,
        mesh: Arc<dyn MeshHandle>,
        events: mpsc::Sender<Event>,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                mesh,
                events,
                listeners: Mutex::new(HashMap::new()),
                transfers: Mutex::new(HashMap::new()),
                presence: Mutex::new(HashSet::new()),
                delivering: Mutex::new(HashSet::new()),
                reschedule: Mutex::new(HashSet::new()),
            }),
        }
    }

    pub fn store(&self) -> &MessengerStore {
        &self.inner.store
    }

    /// Bring every existing account online. Called once, after the core starts.
    pub async fn start(&self) -> Result<()> {
        for account in self.inner.store.accounts()? {
            // A filled pool means the first invite can start a session without
            // waiting for another round trip.
            let _ = self
                .inner
                .store
                .ensure_one_time_prekeys(&account.account_id, ONE_TIME_PREKEY_TARGET);
            self.start_account(&account).await;
        }
        Ok(())
    }

    pub fn shutdown(&self) {
        for (_, listener) in self.inner.listeners.lock().unwrap().drain() {
            let _ = listener.stop.send(true);
        }
    }

    async fn start_account(&self, account: &MessengerAccount) {
        if self
            .inner
            .listeners
            .lock()
            .unwrap()
            .contains_key(&account.account_id)
        {
            return;
        }
        let inner = self.inner.clone();
        let account_id = account.account_id.clone();
        let (stop, mut stopped) = watch::channel(false);
        let online = Arc::new(AtomicBool::new(false));
        self.inner.listeners.lock().unwrap().insert(
            account_id.clone(),
            Listener {
                stop: stop.clone(),
                online: online.clone(),
            },
        );
        tokio::spawn(async move {
            let Ok((listener, port)) = peer::bind_listener().await else {
                inner
                    .notice("Messenger could not open a local listener.")
                    .await;
                return;
            };
            let channel = peer::channel_for(&account_id);
            if let Err(error) = inner.mesh.request(
                "mesh_start",
                &serde_json::json!({"channel": channel, "port": port}),
            ) {
                inner
                    .notice(format!("Messenger Tor request failed: {error}"))
                    .await;
                return;
            }
            let onion = tokio::select! {
                _ = stopped.changed() => return,
                onion = peer::await_onion(inner.mesh.as_ref(), &channel, ONION_BUDGET) => onion,
            };
            let onion = match onion {
                Ok(onion) => onion,
                Err(error) => {
                    inner
                        .notice(format!("Messenger onion is offline: {error}"))
                        .await;
                    return;
                }
            };
            if let Err(error) = inner.store.set_onion(&account_id, &onion) {
                inner
                    .notice(format!("Messenger could not save its onion: {error}"))
                    .await;
                return;
            }
            online.store(true, Ordering::SeqCst);
            inner.emit_accounts().await;
            // Flush anything queued while this account was off.
            spawn_delivery(&inner, &account_id, None);
            spawn_presence(&inner, &account_id, stopped.clone());
            let handler = {
                let inner = inner.clone();
                let account_id = account_id.clone();
                move |stream| {
                    let inner = inner.clone();
                    let account_id = account_id.clone();
                    async move { serve_connection(inner, account_id, stream).await }
                }
            };
            peer::spawn_accept(listener, stopped.clone(), handler);
            let _ = stopped.changed().await;
            online.store(false, Ordering::SeqCst);
        });
    }
}

impl Inner {
    async fn notice(&self, message: impl Into<String>) {
        let _ = self
            .events
            .send(Event::Notice {
                message: message.into(),
            })
            .await;
    }

    fn account_views(&self) -> Vec<serde_json::Value> {
        let online: HashMap<String, bool> = self
            .listeners
            .lock()
            .unwrap()
            .iter()
            .map(|(id, listener)| (id.clone(), listener.online.load(Ordering::SeqCst)))
            .collect();
        self.store
            .accounts()
            .unwrap_or_default()
            .into_iter()
            .map(|account| {
                let is_online = online.get(&account.account_id).copied().unwrap_or(false);
                serde_json::to_value(AccountView::new(&account, is_online)).unwrap_or_default()
            })
            .collect()
    }

    async fn emit(&self, event: Event) {
        let _ = self.events.send(event).await;
    }

    async fn emit_accounts(&self) {
        self.emit(Event::MessengerAccountsChanged {
            accounts: self.account_views(),
        })
        .await;
    }

    async fn emit_conversations(&self, account_id: &str) {
        let conversations = self
            .conversation_views(account_id)
            .unwrap_or_default()
            .iter()
            .map(|view| serde_json::to_value(view).unwrap_or_default())
            .collect();
        self.emit(Event::MessengerConversations {
            account_id: account_id.to_string(),
            conversations,
        })
        .await;
        // Contacts travel with the list: the group member picker needs them, and
        // fetching them separately would be a second chance to go stale.
        let contacts = self
            .store
            .contacts(account_id)
            .unwrap_or_default()
            .iter()
            .map(|contact| serde_json::to_value(contact).unwrap_or_default())
            .collect();
        self.emit(Event::MessengerContacts {
            account_id: account_id.to_string(),
            contacts,
        })
        .await;
    }

    async fn emit_groups(&self, account_id: &str) {
        let groups = self
            .store
            .groups(account_id)
            .unwrap_or_default()
            .iter()
            .map(|group| serde_json::to_value(GroupView::from_record(group)).unwrap_or_default())
            .collect();
        self.emit(Event::MessengerGroups {
            account_id: account_id.to_string(),
            groups,
        })
        .await;
    }

    /// Pending contact requests, so the Chat tab can show "X wants to chat".
    async fn emit_contact_requests(&self, account_id: &str) {
        let requests = self
            .store
            .contact_requests(account_id)
            .unwrap_or_default()
            .iter()
            .map(|row| {
                serde_json::json!({
                    "user_id": row.user_id,
                    "display_name": row.display_name,
                    "onion": row.onion_address,
                    "direction": row.direction,
                    "created_at": row.created_at,
                })
            })
            .collect();
        self.emit(Event::MessengerContactRequests {
            account_id: account_id.to_string(),
            requests,
        })
        .await;
    }

    async fn emit_group(&self, account_id: &str, group: &GroupRecord) {
        self.emit(Event::MessengerGroupMemberUpdate {
            account_id: account_id.to_string(),
            group: serde_json::to_value(GroupView::from_record(group)).unwrap_or_default(),
        })
        .await;
    }

    async fn emit_history(&self, account_id: &str, conversation_id: &str) {
        let messages = self
            .store
            .messages(account_id, conversation_id)
            .unwrap_or_default()
            .iter()
            .map(|record| {
                serde_json::to_value(MessageView::from_record(record)).unwrap_or_default()
            })
            .collect();
        self.emit(Event::MessengerHistory {
            account_id: account_id.to_string(),
            conversation_id: conversation_id.to_string(),
            messages,
        })
        .await;
    }

    async fn emit_status(&self, account_id: &str, record: &MessageRecord) {
        self.emit(Event::MessengerMessageStatus {
            account_id: account_id.to_string(),
            message_id: record.id.clone(),
            status: record.status.as_str().to_string(),
            delivered_to: record.delivered_to,
            total_members: record.total_members,
        })
        .await;
    }

    async fn emit_media(
        &self,
        account_id: &str,
        message_id: &str,
        transfer_id: &str,
        direction: &str,
        file_name: &str,
        received: u32,
        total: u32,
        status: &str,
    ) {
        self.emit(Event::MessengerMediaProgress {
            account_id: account_id.to_string(),
            message_id: message_id.to_string(),
            transfer_id: transfer_id.to_string(),
            direction: direction.to_string(),
            file_name: file_name.to_string(),
            received,
            total,
            status: status.to_string(),
        })
        .await;
    }

    fn conversation_views(&self, account_id: &str) -> Result<Vec<ConversationView>> {
        let contacts: HashMap<String, String> = self
            .store
            .contacts(account_id)?
            .into_iter()
            .map(|contact| (contact.user_id, contact.display_name))
            .collect();
        let groups: HashMap<String, String> = self
            .store
            .groups(account_id)?
            .into_iter()
            .map(|group| (group.group_id, group.name))
            .collect();
        Ok(self
            .store
            .conversations(account_id)?
            .into_iter()
            .map(|row: ConversationSummary| {
                let (title, subtitle) = if row.kind == "group" {
                    (
                        groups
                            .get(&row.target)
                            .cloned()
                            .unwrap_or_else(|| "Group".into()),
                        "Group".to_string(),
                    )
                } else {
                    (
                        contacts
                            .get(&row.target)
                            .cloned()
                            .unwrap_or_else(|| short_id(&row.target)),
                        "Direct".to_string(),
                    )
                };
                ConversationView {
                    conversation_id: row.conversation_id.clone(),
                    kind: row.kind.clone(),
                    target: row.target.clone(),
                    title,
                    subtitle,
                    unread: row.unread,
                    updated_at: row.updated_at,
                    last_kind: row.last_kind.clone(),
                    preview: preview_of(&row),
                }
            })
            .collect())
    }
}

/// A short, stable handle for a user id when no display name is known.
fn short_id(user_id: &str) -> String {
    if user_id.len() > 10 {
        format!("{}…", &user_id[..10])
    } else {
        user_id.to_string()
    }
}

/// The one-line preview in the conversation list, with the attachment badge.
fn preview_of(row: &ConversationSummary) -> String {
    let badge = match row.last_kind.as_str() {
        "image" => "🖼 photo",
        "video" => "🎥 video",
        "audio" => "🎤 audio",
        "file" => "📎 file",
        "system" => "system",
        _ => "",
    };
    if badge.is_empty() {
        return row.last_text.clone();
    }
    if row.last_text.trim().is_empty() {
        badge.to_string()
    } else {
        format!("{badge} · {}", row.last_text)
    }
}

/// `dm:<account>:<peer>` → `<peer>`. The account id cannot contain `:`, so a
/// fixed-prefix strip is both cheaper and stricter than counting separators.
fn direct_peer_of(account_id: &str, conversation_id: &str) -> Option<String> {
    conversation_id
        .strip_prefix("dm:")?
        .strip_prefix(account_id)?
        .strip_prefix(':')
        .map(str::to_string)
}

/// `grp:<account>:<group>` → `<group>`.
fn group_of_conversation(account_id: &str, conversation_id: &str) -> Option<String> {
    conversation_id
        .strip_prefix("grp:")?
        .strip_prefix(account_id)?
        .strip_prefix(':')
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// Ops
// ---------------------------------------------------------------------------

impl MessengerService {
    pub async fn handle_op(&self, op: Op) -> Result<()> {
        let inner = self.inner.clone();
        match op {
            // ------------------------- accounts -------------------------
            Op::MessengerCreateAccount { display_name, pro } => {
                let account = inner.store.create_account(&display_name, pro)?;
                inner
                    .store
                    .ensure_one_time_prekeys(&account.account_id, ONE_TIME_PREKEY_TARGET)?;
                self.start_account(&account).await;
                inner.emit_accounts().await;
            }
            Op::MessengerListAccounts => inner.emit_accounts().await,
            Op::MessengerSwitchAccount { account_id } => {
                let account = inner
                    .store
                    .account(&account_id)?
                    .context("that messenger account does not exist")?;
                inner.store.set_active(&account_id)?;
                self.start_account(&account).await;
                inner.emit_accounts().await;
                inner.emit_conversations(&account_id).await;
                inner.emit_groups(&account_id).await;
            }
            Op::MessengerDeleteAccount { account_id } => {
                if let Some(listener) = inner.listeners.lock().unwrap().remove(&account_id) {
                    let _ = listener.stop.send(true);
                }
                let _ = inner.mesh.request(
                    "mesh_revoke",
                    &serde_json::json!({"channel": peer::channel_for(&account_id)}),
                );
                inner.store.delete_account(&account_id)?;
                inner.emit_accounts().await;
            }
            Op::MessengerGetInvitePayload { account_id } => {
                let account = inner
                    .store
                    .account(&account_id)?
                    .context("that messenger account does not exist")?;
                inner
                    .emit(Event::MessengerInvite {
                        account_id,
                        payload: invite_payload(&account),
                    })
                    .await;
            }
            Op::MessengerSetProfile {
                account_id,
                display_name,
                avatar_base64,
            } => {
                let avatar = avatar_base64
                    .map(|encoded| {
                        base64::engine::general_purpose::STANDARD
                            .decode(encoded.trim())
                            .context("the avatar is not valid base64")
                    })
                    .transpose()?;
                inner
                    .store
                    .set_profile(&account_id, &display_name, avatar)?;
                inner.emit_accounts().await;
            }
            Op::MessengerSetTier {
                account_id,
                pro,
                chunk_size,
            } => {
                let current = inner
                    .store
                    .account(&account_id)?
                    .context("that messenger account does not exist")?;
                inner
                    .store
                    .set_tier(&account_id, pro, chunk_size.unwrap_or(current.chunk_size))?;
                inner.emit_accounts().await;
            }

            // --------------------- contacts and 1:1 --------------------
            Op::MessengerAddContact { account_id, invite } => {
                let contact = parse_invite(&invite)?;
                let account = inner
                    .store
                    .account(&account_id)?
                    .context("that messenger account does not exist")?;
                if contact.user_id == account.user_id {
                    bail!("that invite belongs to this account");
                }
                // Scanning is a request, not a decision for the other side. The
                // scanner may keep the contact locally so its own composer works
                // and its messages queue as Pending; what it may *not* do is make
                // the peer a contact on the other device. That side sees the
                // request and answers it in its own Chat tab.
                inner.store.upsert_contact(&account_id, &contact)?;
                inner
                    .store
                    .upsert_contact_request(&account_id, &contact, &invite, "outgoing")?;
                let conversation = inner
                    .store
                    .direct_conversation(&account_id, &contact.user_id)?;
                inner.emit_conversations(&account_id).await;
                inner.emit_history(&account_id, &conversation).await;
                let own_invite = invite_payload(&account).to_string();
                let outcome = send_contact_request(&inner, &account, &contact, &own_invite).await;
                inner.emit_contact_requests(&account_id).await;
                match outcome {
                    ContactOutcome::Accepted => {
                        inner.store.upsert_contact(&account_id, &contact)?;
                        inner
                            .store
                            .remove_contact_request(&account_id, &contact.user_id)?;
                        inner.emit_conversations(&account_id).await;
                        inner.emit_contact_requests(&account_id).await;
                        inner
                            .notice(format!(
                                "{} accepted you. You can chat now.",
                                contact.display_name
                            ))
                            .await;
                    }
                    ContactOutcome::Pending => {
                        inner
                            .notice(format!(
                                "Request sent to {}. Waiting for them to accept in their Chat tab.",
                                contact.display_name
                            ))
                            .await;
                    }
                    ContactOutcome::Unreachable(reason) => {
                        inner
                            .notice(format!(
                                "Request saved for {}. It will be sent when their device is reachable ({reason}).",
                                contact.display_name
                            ))
                            .await;
                    }
                }
            }
            Op::MessengerListContactRequests { account_id } => {
                inner.emit_contact_requests(&account_id).await
            }
            Op::MessengerRespondContactRequest {
                account_id,
                user_id,
                accept,
            } => {
                let account = inner
                    .store
                    .account(&account_id)?
                    .context("that messenger account does not exist")?;
                let requests = inner.store.contact_requests(&account_id)?;
                let Some(request) = requests.iter().find(|row| row.user_id == user_id) else {
                    bail!("that contact request no longer exists");
                };
                let contact = parse_invite(&request.invite_json)?;
                let own_invite = invite_payload(&account).to_string();
                // Answer first: the sender learns the decision even if our own
                // local write is the slower step.
                let delivered =
                    send_contact_response(&inner, &contact, &account, accept, &own_invite).await;
                if accept {
                    inner.store.upsert_contact(&account_id, &contact)?;
                    let conversation = inner
                        .store
                        .direct_conversation(&account_id, &contact.user_id)?;
                    inner.emit_history(&account_id, &conversation).await;
                    // Anything this person sent before being accepted is still
                    // queued. Flush it now instead of waiting for the offline
                    // retry timer: the answer they are waiting for is one of
                    // those messages.
                    spawn_delivery(&inner, &account_id, Some(user_id.clone()));
                }
                inner.store.remove_contact_request(&account_id, &user_id)?;
                inner
                    .store
                    .remove_contact_request_note(&account_id, &user_id)?;
                inner
                    .emit_history(
                        &account_id,
                        &inner.store.direct_conversation(&account_id, &user_id)?,
                    )
                    .await;
                inner.emit_conversations(&account_id).await;
                inner.emit_contact_requests(&account_id).await;
                inner
                    .notice(match (accept, delivered) {
                        (true, true) => format!("{} is now a contact.", contact.display_name),
                        (true, false) => format!(
                            "{} added locally; they were offline and will see the acceptance later.",
                            contact.display_name
                        ),
                        (false, true) => format!("Declined {}.", contact.display_name),
                        (false, false) => format!(
                            "Declined {} locally; they were offline.",
                            contact.display_name
                        ),
                    })
                    .await;
            }
            Op::MessengerListConversations { account_id } => {
                inner.emit_conversations(&account_id).await;
            }
            Op::MessengerGetMessages {
                account_id,
                conversation_id,
            } => inner.emit_history(&account_id, &conversation_id).await,
            Op::MessengerSendText {
                account_id,
                to,
                text,
                reply_to,
                group,
            } => {
                let text = text.trim().to_string();
                if text.is_empty() {
                    bail!("a message cannot be empty");
                }
                if group {
                    self.send_group(
                        &account_id,
                        &to,
                        MessageKind::Text,
                        Vec::new(),
                        &text,
                        reply_to,
                        None,
                    )
                    .await?;
                } else {
                    self.send_direct(
                        &account_id,
                        &to,
                        MessageKind::Text,
                        Vec::new(),
                        &text,
                        reply_to,
                    )
                    .await?;
                }
            }
            Op::MessengerSendMedia {
                account_id,
                to,
                path,
                paths,
                caption,
                reply_to,
                group,
            } => {
                let account = inner
                    .store
                    .account(&account_id)?
                    .context("that messenger account does not exist")?;
                // One message may carry any number of files. A single `path` is
                // the legacy field and is folded in first so an older client
                // keeps working unchanged.
                let mut selected: Vec<std::path::PathBuf> = Vec::new();
                if let Some(single) = path {
                    selected.push(single);
                }
                selected.extend(paths);
                if selected.is_empty() {
                    bail!("no file was selected");
                }
                let pro = account.pro;
                let chunk = account.chunk_size;
                let mut prepared = Vec::with_capacity(selected.len());
                for file in selected {
                    let (outgoing, meta) = tokio::task::spawn_blocking(move || {
                        media::prepare(&file, chunk, pro).map(|outgoing| {
                            let meta = AttachmentMeta {
                                mime: outgoing.mime.clone(),
                                file_name: outgoing.file_name.clone(),
                                size_bytes: outgoing.size_bytes,
                                duration_ms: None,
                                width: None,
                                height: None,
                                sha256: outgoing.sha256.clone(),
                                chunk_size: outgoing.chunk_size,
                                total_chunks: outgoing.chunks,
                            };
                            (outgoing, meta)
                        })
                    })
                    .await
                    .context("attachment preparation failed")??;
                    prepared.push((outgoing, meta));
                }
                // A mixed selection still gets one kind; text plus files is
                // `file` unless every file is an image, so the badge never lies
                // about a document.
                let kind = if prepared
                    .iter()
                    .all(|(_, meta)| meta.mime.starts_with("image/"))
                {
                    MessageKind::Image
                } else if prepared.len() == 1 {
                    MessageKind::from_mime(&prepared[0].1.mime)
                } else {
                    MessageKind::File
                };
                let text = caption.unwrap_or_default();
                if group {
                    self.send_group(&account_id, &to, kind, prepared, &text, reply_to, None)
                        .await?;
                } else {
                    self.send_direct(&account_id, &to, kind, prepared, &text, reply_to)
                        .await?;
                }
            }
            Op::MessengerEditMessage {
                account_id,
                id,
                new_text,
            } => {
                let new_text = new_text.trim().to_string();
                if new_text.is_empty() {
                    bail!("an edit cannot be empty");
                }
                let Some(record) = inner.store.edit_message(&account_id, &id, &new_text)? else {
                    bail!("that message no longer exists");
                };
                inner
                    .emit(Event::MessengerMessageEdited {
                        account_id: account_id.clone(),
                        message: serde_json::to_value(MessageView::from_record(&record))
                            .unwrap_or_default(),
                    })
                    .await;
                if let Some(group_id) = group_of_conversation(&account_id, &record.conversation_id)
                {
                    self.send_group(
                        &account_id,
                        &group_id,
                        MessageKind::Edit,
                        Vec::new(),
                        "",
                        None,
                        Some(record.id),
                    )
                    .await?;
                } else if let Some(peer_id) = direct_peer_of(&account_id, &record.conversation_id) {
                    self.send_direct_edit(&account_id, &peer_id, &record, &new_text)?;
                }
                inner.emit_conversations(&account_id).await;
            }
            Op::MessengerMarkRead {
                account_id,
                conversation_id,
            } => {
                inner.store.clear_unread(&account_id, &conversation_id)?;
                inner.emit_conversations(&account_id).await;
                self.send_read_receipts(&account_id, &conversation_id)?;
            }

            // --------------------------- groups -------------------------
            Op::MessengerCreateGroup {
                account_id,
                name,
                member_ids,
            } => {
                let account = inner
                    .store
                    .account(&account_id)?
                    .context("that messenger account does not exist")?;
                let members = self.members_from_contacts(&account_id, &account, &member_ids)?;
                let group =
                    inner
                        .store
                        .create_group(&account_id, &name, &account.user_id, members)?;
                inner.emit_groups(&account_id).await;
                inner.emit_conversations(&account_id).await;
                self.system_message(
                    &account_id,
                    &group.group_id,
                    &format!("{} created the group", account.display_name),
                );
                // Every member receives the group state and this account's
                // sender key over their own 1:1 channel — never on a broadcast.
                self.distribute_sender_key(&account_id, &group.group_id)
                    .await?;
            }
            Op::MessengerAddGroupMembers {
                account_id,
                group_id,
                member_ids,
            } => {
                let account = inner
                    .store
                    .account(&account_id)?
                    .context("that messenger account does not exist")?;
                let Some(mut group) = inner.store.group(&account_id, &group_id)? else {
                    bail!("that group does not exist");
                };
                if !group.is_admin(&account.user_id) {
                    bail!("only a group admin can add members");
                }
                for addition in self.members_from_contacts(&account_id, &account, &member_ids)? {
                    if group.member(&addition.user_id).is_none() {
                        group.members.push(addition);
                    }
                }
                if group.members.len() > MAX_GROUP_MEMBERS {
                    bail!("a group is limited to {MAX_GROUP_MEMBERS} members");
                }
                inner.store.save_group(&group)?;
                inner.emit_groups(&account_id).await;
                inner.emit_group(&account_id, &group).await;
                self.system_message(
                    &account_id,
                    &group_id,
                    &format!("{} added members", account.display_name),
                );
                self.distribute_sender_key(&account_id, &group_id).await?;
            }
            Op::MessengerRemoveGroupMember {
                account_id,
                group_id,
                user_id,
            } => {
                let account = inner
                    .store
                    .account(&account_id)?
                    .context("that messenger account does not exist")?;
                let Some(mut group) = inner.store.group(&account_id, &group_id)? else {
                    bail!("that group does not exist");
                };
                if !group.is_admin(&account.user_id) {
                    bail!("only a group admin can remove a member");
                }
                if user_id == account.user_id {
                    bail!("leaving a group is a separate action");
                }
                group.members.retain(|member| member.user_id != user_id);
                group.admins.retain(|admin| admin != &user_id);
                inner.store.save_group(&group)?;
                // Rotate: the removed member must not decrypt anything after
                // this point, so all sender chains for this group are dropped.
                inner.store.drop_sender_chains(&account_id, &group_id)?;
                inner.emit_groups(&account_id).await;
                inner.emit_group(&account_id, &group).await;
                self.system_message(
                    &account_id,
                    &group_id,
                    &format!("{} removed a member", account.display_name),
                );
                self.distribute_sender_key(&account_id, &group_id).await?;
            }
            Op::MessengerListGroups { account_id } => inner.emit_groups(&account_id).await,
            Op::MessengerGetGroupMessages {
                account_id,
                group_id,
            } => {
                let conversation = inner.store.group_conversation(&account_id, &group_id)?;
                inner.emit_history(&account_id, &conversation).await;
            }
            Op::MessengerFlushQueue { account_id } => {
                spawn_delivery(&inner, &account_id, None);
                inner.emit_accounts().await;
            }
            // Never format the op: it can carry message text or an invite.
            _ => bail!("that operation does not belong to the messenger"),
        }
        Ok(())
    }
}

/// The invite a peer scans. It carries exactly what X3DH needs and the account's
/// own onion — nothing about the device, the mesh or the operating system.
fn invite_payload(account: &MessengerAccount) -> serde_json::Value {
    serde_json::json!({
        "v": 1,
        "user_id": account.user_id,
        "identity_pk": account.identity_pk,
        "identity_public": account.identity_public,
        "signed_prekey": account.signed_prekey,
        "spk_sig": account.prekey_signature,
        "onion": account.onion_address,
        "name": account.display_name,
    })
}

/// Validate an invite before it can become a contact.
///
/// `user_id` must really be the identity key, and the signed prekey must carry
/// that identity's signature. Without both checks a peer could hand us a name
/// it does not own and a prekey it does not control.
fn parse_invite(invite: &str) -> Result<MessengerContact> {
    let parsed: serde_json::Value =
        serde_json::from_str(invite.trim()).context("that is not a messenger invite")?;
    if parsed["v"].as_u64() != Some(1) {
        bail!("this invite was written by a different messenger version");
    }
    let field = |key: &str| -> Result<String> {
        parsed[key]
            .as_str()
            .map(str::to_string)
            .with_context(|| format!("the invite is missing `{key}`"))
    };
    let user_id = field("user_id")?;
    let identity_public = field("identity_public")?;
    let identity_pk = field("identity_pk")?;
    let signed_prekey = field("signed_prekey")?;
    let signature = field("spk_sig")?;
    let onion = field("onion")?;
    let name = parsed["name"].as_str().unwrap_or("").trim().to_string();
    if hex::decode(&user_id).map(|bytes| bytes.len()) != Ok(32) {
        bail!("the invite's user id is not a key");
    }
    let signing = crypto::verifying_key(&identity_public)?;
    if hex::encode(signing.to_bytes()) != user_id {
        bail!("the invite's user id does not match its identity key");
    }
    let spk = hex::decode(&signed_prekey).context("the invite's prekey is malformed")?;
    crypto::verify(
        &signing,
        &[b"gnomeai-messenger-signed-prekey-v1".as_slice(), &spk].concat(),
        &signature,
    )
    .context("the invite's prekey is not signed by its identity")?;
    if !onion.is_empty() && !peer::valid_onion_host(&onion) {
        bail!("the invite's onion address is not valid");
    }
    Ok(MessengerContact {
        user_id,
        display_name: if name.is_empty() {
            "Contact".into()
        } else {
            name
        },
        identity_pk,
        signed_prekey,
        onion_address: onion,
        verified: true,
    })
}

// ---------------------------------------------------------------------------
// Outgoing messages
// ---------------------------------------------------------------------------

/// What the outbox carries. It is sealed in the database, and it is the only
/// thing a delivery worker needs: which peer, which message row, and what to
/// say if the peer has to be asked for its prekeys first.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct QueuedItem {
    /// `direct`, `group`, `sender_key` or `receipt`.
    kind: String,
    /// Peer user id for direct traffic, group id for group traffic.
    target: String,
    /// The message row this item belongs to, when there is one.
    #[serde(default)]
    message_id: String,
    #[serde(default)]
    body: Option<MessageBody>,
    #[serde(default)]
    group_envelope: Option<GroupEnvelope>,
    #[serde(default)]
    distribution: Option<SenderKeyDistribution>,
    #[serde(default)]
    receipt: Option<Receipt>,
    /// Local files that follow the control message, in body order. A message
    /// may carry any number of them; each one is streamed on its own.
    #[serde(default)]
    attachments: Vec<QueuedAttachment>,
    /// Legacy single-attachment fields, still read so an item queued by an
    /// older build is delivered instead of failing to parse.
    #[serde(default, skip_serializing)]
    attachment_path: Option<String>,
    #[serde(default, skip_serializing)]
    attachment_key: Option<String>,
}

/// One prepared local file waiting to be streamed for a queued message.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct QueuedAttachment {
    path: String,
    /// Hex of the per-attachment key, already present in the body.
    key: String,
    /// Position inside the body's attachment list; the transfer id derives from
    /// it so chunks of different files never collide.
    index: u32,
}

impl QueuedItem {
    /// Every attachment this item still has to stream, legacy field included.
    fn attachment_queue(&self) -> Vec<QueuedAttachment> {
        if !self.attachments.is_empty() {
            return self.attachments.clone();
        }
        match (&self.attachment_path, &self.attachment_key) {
            (Some(path), Some(key)) => vec![QueuedAttachment {
                path: path.clone(),
                key: key.clone(),
                index: 0,
            }],
            _ => Vec::new(),
        }
    }
}

/// Transfer id of one attachment inside a message. The first file keeps the
/// message id so a single-attachment message is wire-compatible with older
/// builds; later ones append their index.
fn part_transfer_id(message_id: &str, index: u32) -> String {
    if index == 0 {
        message_id.to_string()
    } else {
        format!("{message_id}:{index}")
    }
}

/// The `media` JSON the interface renders.
///
/// One attachment keeps the historical shape — a bare metadata object — so an
/// existing row, draft or older peer renders unchanged. Two or more become
/// `{"parts": [meta, …]}`, which the transcript expands into one card per file.
fn media_json_for(attachments: &[(media::Outgoing, AttachmentMeta)]) -> Result<Option<String>> {
    match attachments.len() {
        0 => Ok(None),
        1 => Ok(Some(serde_json::to_string(&attachments[0].1)?)),
        _ => Ok(Some(serde_json::to_string(&serde_json::json!({
            "parts": attachments
                .iter()
                .map(|(_, meta)| meta.clone())
                .collect::<Vec<_>>(),
        }))?)),
    }
}

/// The same shape for a message we received: every part's metadata, with one
/// part keeping the historical bare-object form.
fn incoming_media_json(body: &MessageBody) -> Result<Option<String>> {
    let parts = body.parts();
    match parts.len() {
        0 => Ok(None),
        1 => Ok(Some(serde_json::to_string(&parts[0].meta)?)),
        _ => Ok(Some(serde_json::to_string(&serde_json::json!({
            "parts": parts
                .iter()
                .map(|part| part.meta.clone())
                .collect::<Vec<_>>(),
        }))?)),
    }
}

impl MessengerService {
    /// Record the row first, then queue. A crash between the two leaves a
    /// visible `Pending` message rather than a message that silently vanished.
    #[allow(clippy::too_many_arguments)]
    async fn send_direct(
        &self,
        account_id: &str,
        peer_user_id: &str,
        kind: MessageKind,
        attachments: Vec<(media::Outgoing, AttachmentMeta)>,
        text: &str,
        reply_to: Option<String>,
    ) -> Result<()> {
        self.queue_direct(
            account_id,
            peer_user_id,
            kind,
            attachments,
            text,
            reply_to,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn queue_direct(
        &self,
        account_id: &str,
        peer_user_id: &str,
        kind: MessageKind,
        attachments: Vec<(media::Outgoing, AttachmentMeta)>,
        text: &str,
        reply_to: Option<String>,
        edit_of: Option<String>,
    ) -> Result<()> {
        let account = self
            .inner
            .store
            .account(account_id)?
            .context("that messenger account does not exist")?;
        let conversation = self
            .inner
            .store
            .direct_conversation(account_id, peer_user_id)?;
        let message_id = crypto::random_id();
        // The body carries every attachment's metadata and key; the queue keeps
        // the local paths. Splitting them here is what lets one message hold any
        // number of files without duplicating the secret in two places.
        let parts: Vec<AttachmentPart> = attachments
            .iter()
            .map(|(prepared, meta)| AttachmentPart {
                meta: meta.clone(),
                key: hex::encode(prepared.attachment_key()),
            })
            .collect();
        let body = MessageBody {
            message_id: message_id.clone(),
            kind,
            text: (!text.is_empty()).then(|| text.to_string()),
            attachments: parts,
            attachment: None,
            attachment_key: None,
            reply_to: reply_to.clone(),
            edit_of,
            group_id: None,
            sender_id: None,
            sender_name: None,
            iteration: None,
            sent_at: chrono::Utc::now().timestamp_millis(),
        };
        self.inner.store.insert_message(&NewMessage {
            id: message_id.clone(),
            conversation_id: conversation.clone(),
            account_id: account_id.to_string(),
            direction: "outgoing".into(),
            from_user_id: account.user_id.clone(),
            to_user_id: peer_user_id.to_string(),
            ts: body.sent_at,
            kind: kind.as_str().into(),
            text: body.text.clone(),
            reply_to,
            media_json: media_json_for(&attachments)?,
            status: MessageStatus::Pending,
            ratchet_header: None,
            ciphertext: None,
            signature: None,
            delivered_to: 0,
            total_members: 0,
        })?;
        for (index, (prepared, meta)) in attachments.iter().enumerate() {
            self.inner.store.start_transfer(
                &part_transfer_id(&message_id, index as u32),
                account_id,
                peer_user_id,
                "outgoing",
                &message_id,
                &prepared.path.display().to_string(),
                &serde_json::to_string(meta)?,
                prepared.attachment_key(),
            )?;
        }
        // The sender's own interface learns about the message right now.
        // Without this event the bubble only appeared after the next full
        // refresh (reopen), and the conversation list kept its old preview.
        // Emitted before the queue item consumes `message_id`.
        if let Some(record) = self.inner.store.message(&message_id)? {
            self.inner
                .emit(Event::MessengerMessageReceived {
                    account_id: account_id.to_string(),
                    message: serde_json::to_value(MessageView::from_record(&record))
                        .unwrap_or_default(),
                })
                .await;
        }
        self.inner.emit_conversations(account_id).await;
        let item = QueuedItem {
            kind: "direct".into(),
            target: peer_user_id.to_string(),
            message_id,
            body: Some(body),
            group_envelope: None,
            distribution: None,
            receipt: None,
            attachments: attachments
                .iter()
                .enumerate()
                .map(|(index, (prepared, _))| QueuedAttachment {
                    path: prepared.path.display().to_string(),
                    key: hex::encode(prepared.attachment_key()),
                    index: index as u32,
                })
                .collect(),
            attachment_path: None,
            attachment_key: None,
        };
        self.inner.store.enqueue(
            account_id,
            &item.message_id,
            peer_user_id,
            &serde_json::to_string(&item)?,
        )?;
        spawn_delivery(&self.inner, account_id, Some(peer_user_id.to_string()));
        Ok(())
    }

    fn send_direct_edit(
        &self,
        account_id: &str,
        peer_user_id: &str,
        record: &MessageRecord,
        new_text: &str,
    ) -> Result<()> {
        let item = QueuedItem {
            kind: "direct".into(),
            target: peer_user_id.to_string(),
            message_id: record.id.clone(),
            body: Some(MessageBody {
                message_id: crypto::random_id(),
                kind: MessageKind::Edit,
                text: Some(new_text.to_string()),
                attachments: Vec::new(),
                attachment: None,
                attachment_key: None,
                reply_to: None,
                edit_of: Some(record.id.clone()),
                group_id: None,
                sender_id: None,
                sender_name: None,
                iteration: None,
                sent_at: chrono::Utc::now().timestamp_millis(),
            }),
            group_envelope: None,
            distribution: None,
            receipt: None,
            attachments: Vec::new(),
            attachment_path: None,
            attachment_key: None,
        };
        self.inner.store.enqueue(
            account_id,
            &record.id,
            peer_user_id,
            &serde_json::to_string(&item)?,
        )?;
        spawn_delivery(&self.inner, account_id, Some(peer_user_id.to_string()));
        Ok(())
    }

    /// `Read` receipts are only sent when a direct channel exists; there is no
    /// server to hold them, so a missed receipt is simply never claimed.
    fn send_read_receipts(&self, account_id: &str, conversation_id: &str) -> Result<()> {
        let Some(peer_id) = direct_peer_of(account_id, conversation_id) else {
            return Ok(());
        };
        if self.inner.store.contact(account_id, &peer_id)?.is_none() {
            return Ok(());
        }
        let unread: Vec<String> = self
            .inner
            .store
            .messages(account_id, conversation_id)?
            .into_iter()
            .filter(|message| {
                message.direction == "incoming" && message.status != MessageStatus::Read
            })
            .map(|message| message.id)
            .collect();
        if unread.is_empty() {
            return Ok(());
        }
        for id in unread {
            self.inner.store.update_status(&id, MessageStatus::Read)?;
            let item = QueuedItem {
                kind: "receipt".into(),
                target: peer_id.clone(),
                message_id: id.clone(),
                body: None,
                group_envelope: None,
                distribution: None,
                receipt: Some(Receipt {
                    message_id: id,
                    status: "read".into(),
                }),
                attachments: Vec::new(),
                attachment_path: None,
                attachment_key: None,
            };
            self.inner
                .store
                .enqueue(account_id, "", &peer_id, &serde_json::to_string(&item)?)?;
        }
        spawn_delivery(&self.inner, account_id, Some(peer_id));
        Ok(())
    }
}

/// How a scanned invite ended.
enum ContactOutcome {
    /// They had already added us, so the handshake completed immediately.
    Accepted,
    /// Delivered, and a person on the other device still has to answer.
    Pending,
    /// Their device did not answer; the request stays on disk.
    Unreachable(String),
}

/// Send the first half of a handshake: "I scanned your QR, may we chat?"
///
/// The reply decides what happens next. `ContactAnswer` means they already had
/// us as a contact, so the introduction is mutual at once; `ContactPending` means a
/// person still has to press Accept on the other device.
async fn send_contact_request(
    inner: &Arc<Inner>,
    account: &MessengerAccount,
    contact: &MessengerContact,
    own_invite: &str,
) -> ContactOutcome {
    send_contact_request_with(inner, account, contact, own_invite, DELIVERY_TIMEOUT).await
}

/// The same handshake with an explicit deadline, so a retry inside the
/// presence tick cannot hold the loop for the full delivery budget.
async fn send_contact_request_with(
    inner: &Arc<Inner>,
    account: &MessengerAccount,
    contact: &MessengerContact,
    own_invite: &str,
    timeout: Duration,
) -> ContactOutcome {
    if contact.onion_address.is_empty() {
        return ContactOutcome::Unreachable("their invite carries no onion".into());
    }
    let socks = match peer::socks_port(inner.mesh.as_ref()).await {
        Ok(port) => port,
        Err(error) => return ContactOutcome::Unreachable(error.to_string()),
    };
    let request = Request::ContactRequest {
        invite: own_invite.to_string(),
    };
    match peer::exchange(socks, &contact.onion_address, &request, timeout).await {
        Ok(Reply::ContactPending { .. }) => ContactOutcome::Pending,
        Ok(Reply::ContactAnswer {
            accepted, invite, ..
        }) => {
            if !accepted {
                return ContactOutcome::Unreachable("their device declined".into());
            }
            // Their answer carries their invite, so accepting it here makes
            // the introduction mutual without another round trip.
            if let Ok(answered) = parse_invite(&invite) {
                let _ = inner.store.upsert_contact(&account.account_id, &answered);
            }
            ContactOutcome::Accepted
        }
        Ok(Reply::Accepted) => ContactOutcome::Pending,
        Ok(Reply::Error { message }) => ContactOutcome::Unreachable(message),
        Ok(_) => ContactOutcome::Pending,
        Err(error) => ContactOutcome::Unreachable(error.to_string()),
    }
}

/// Answer a request we received. Returns whether the peer learned about it.
async fn send_contact_response(
    inner: &Arc<Inner>,
    contact: &MessengerContact,
    account: &MessengerAccount,
    accept: bool,
    own_invite: &str,
) -> bool {
    if contact.onion_address.is_empty() {
        return false;
    }
    let Ok(socks) = peer::socks_port(inner.mesh.as_ref()).await else {
        return false;
    };
    let request = Request::ContactResponse {
        user_id: account.user_id.clone(),
        accepted: accept,
        invite: own_invite.to_string(),
    };
    matches!(
        peer::exchange(socks, &contact.onion_address, &request, DELIVERY_TIMEOUT).await,
        Ok(Reply::Accepted)
    )
}

impl MessengerService {
    /// A group message is sealed once under our own sender chain; the same
    /// ciphertext then goes to each member over that member's own onion. That
    /// is the entire fan-out — O(n) connections and no relay.
    #[allow(clippy::too_many_arguments)]
    async fn send_group(
        &self,
        account_id: &str,
        group_id: &str,
        kind: MessageKind,
        attachments: Vec<(media::Outgoing, AttachmentMeta)>,
        text: &str,
        reply_to: Option<String>,
        edit_of: Option<String>,
    ) -> Result<()> {
        let account = self
            .inner
            .store
            .account(account_id)?
            .context("that messenger account does not exist")?;
        let group = self
            .inner
            .store
            .group(account_id, group_id)?
            .context("that group does not exist")?;
        let conversation = self.inner.store.group_conversation(account_id, group_id)?;
        let peers = group.peers(&account.user_id);
        let total = peers.len() as u32;
        let message_id = if edit_of.is_some() {
            edit_of.clone().unwrap_or_default()
        } else {
            crypto::random_id()
        };
        let parts: Vec<AttachmentPart> = attachments
            .iter()
            .map(|(prepared, meta)| AttachmentPart {
                meta: meta.clone(),
                key: hex::encode(prepared.attachment_key()),
            })
            .collect();
        let body = MessageBody {
            message_id: message_id.clone(),
            kind,
            text: (!text.is_empty()).then(|| text.to_string()),
            attachments: parts,
            attachment: None,
            attachment_key: None,
            reply_to: reply_to.clone(),
            edit_of: edit_of.clone(),
            group_id: Some(group_id.to_string()),
            sender_id: Some(account.user_id.clone()),
            sender_name: Some(account.display_name.clone()),
            iteration: None,
            sent_at: chrono::Utc::now().timestamp_millis(),
        };
        if edit_of.is_none() {
            self.inner.store.insert_message(&NewMessage {
                id: message_id.clone(),
                conversation_id: conversation,
                account_id: account_id.to_string(),
                direction: "outgoing".into(),
                from_user_id: account.user_id.clone(),
                to_user_id: String::new(),
                ts: body.sent_at,
                kind: kind.as_str().into(),
                text: body.text.clone(),
                reply_to,
                media_json: media_json_for(&attachments)?,
                status: MessageStatus::Pending,
                ratchet_header: None,
                ciphertext: None,
                signature: None,
                delivered_to: 0,
                total_members: total,
            })?;
        }
        for (index, (prepared, meta)) in attachments.iter().enumerate() {
            // A group transfer has many recipients, so it records no single
            // peer: the chunk check skips the peer comparison for it and the
            // attachment key — which only the members ever see — is what
            // authenticates each chunk.
            self.inner.store.start_transfer(
                &part_transfer_id(&message_id, index as u32),
                account_id,
                "",
                "outgoing",
                &message_id,
                &prepared.path.display().to_string(),
                &serde_json::to_string(meta)?,
                prepared.attachment_key(),
            )?;
        }
        let envelope = self.seal_group(account_id, group_id, &account, &body)?;
        for peer in &peers {
            let item = QueuedItem {
                kind: "group".into(),
                target: group_id.to_string(),
                message_id: message_id.clone(),
                body: Some(body.clone()),
                group_envelope: Some(envelope.clone()),
                distribution: None,
                receipt: None,
                attachments: attachments
                    .iter()
                    .enumerate()
                    .map(|(index, (prepared, _))| QueuedAttachment {
                        path: prepared.path.display().to_string(),
                        key: hex::encode(prepared.attachment_key()),
                        index: index as u32,
                    })
                    .collect(),
                attachment_path: None,
                attachment_key: None,
            };
            self.inner.store.enqueue(
                account_id,
                &message_id,
                &peer.user_id,
                &serde_json::to_string(&item)?,
            )?;
        }
        spawn_delivery(&self.inner, account_id, None);
        // Same as the direct path: the sender's interface learns the message
        // now, not after the next refresh.
        if let Some(record) = self.inner.store.message(&message_id)? {
            self.inner
                .emit(Event::MessengerMessageReceived {
                    account_id: account_id.to_string(),
                    message: serde_json::to_value(MessageView::from_record(&record))
                        .unwrap_or_default(),
                })
                .await;
        }
        self.inner.emit_conversations(account_id).await;
        Ok(())
    }

    /// Seal a group body under the account's sender chain for that group.
    fn seal_group(
        &self,
        account_id: &str,
        group_id: &str,
        account: &MessengerAccount,
        body: &MessageBody,
    ) -> Result<GroupEnvelope> {
        let mut chain = self
            .inner
            .store
            .sender_chain(account_id, group_id, &account.user_id)?
            .unwrap_or_else(|| SenderChain::new(crypto::random_key()));
        let (iteration, key) = chain.advance();
        self.inner
            .store
            .save_sender_chain(account_id, group_id, &account.user_id, &chain)?;
        let plaintext = serde_json::to_vec(body)?;
        let signature = crypto::sign(
            &self
                .inner
                .store
                .identity_secrets(account_id)?
                .identity
                .signing(),
            &format!(
                "group-envelope/v1/{group_id}:{}:{iteration}",
                account.user_id
            )
            .into_bytes(),
        );
        Ok(GroupEnvelope {
            v: PROTOCOL_VERSION,
            group_id: group_id.to_string(),
            sender_id: account.user_id.clone(),
            sender_name: Some(account.display_name.clone()),
            iteration,
            ciphertext: base64::engine::general_purpose::STANDARD.encode(crypto::seal(
                &key,
                &plaintext,
                group_id.as_bytes(),
            )),
            signature,
        })
    }
}

impl MessengerService {
    fn members_from_contacts(
        &self,
        account_id: &str,
        account: &MessengerAccount,
        ids: &[String],
    ) -> Result<Vec<GroupMemberWire>> {
        let mut members = vec![GroupMemberWire {
            user_id: account.user_id.clone(),
            onion: account.onion_address.clone(),
            display_name: account.display_name.clone(),
            identity_pk: account.identity_pk.clone(),
            signed_prekey: account.signed_prekey.clone(),
        }];
        for id in ids {
            let Some(contact) = self.inner.store.contact(account_id, id)? else {
                bail!("`{id}` is not one of this account's contacts");
            };
            if members.iter().any(|member| &member.user_id == id) {
                continue;
            }
            members.push(GroupMemberWire {
                user_id: contact.user_id,
                onion: contact.onion_address,
                display_name: contact.display_name,
                identity_pk: contact.identity_pk,
                signed_prekey: contact.signed_prekey,
            });
        }
        if members.len() > MAX_GROUP_MEMBERS {
            bail!("a group is limited to {MAX_GROUP_MEMBERS} members");
        }
        Ok(members)
    }

    /// A local-only note that explains a membership change inside a transcript.
    fn system_message(&self, account_id: &str, group_id: &str, text: &str) {
        let Ok(conversation) = self.inner.store.group_conversation(account_id, group_id) else {
            return;
        };
        let _ = self.inner.store.insert_message(&NewMessage {
            id: crypto::random_id(),
            conversation_id: conversation,
            account_id: account_id.to_string(),
            direction: "outgoing".into(),
            from_user_id: String::new(),
            to_user_id: String::new(),
            ts: chrono::Utc::now().timestamp_millis(),
            kind: "system".into(),
            text: Some(text.to_string()),
            reply_to: None,
            media_json: None,
            status: MessageStatus::Sent,
            ratchet_header: None,
            ciphertext: None,
            signature: None,
            delivered_to: 0,
            total_members: 0,
        });
    }

    /// Push our sender chain to every member over their own 1:1 channel,
    /// ratcheted. The chain travels *inside* the X3DH/Double-Ratchet envelope,
    /// so only that member can install it — never all members at once, and
    /// never in the clear.
    async fn distribute_sender_key(&self, account_id: &str, group_id: &str) -> Result<()> {
        let account = self
            .inner
            .store
            .account(account_id)?
            .context("that messenger account does not exist")?;
        let Some(group) = self.inner.store.group(account_id, group_id)? else {
            bail!("that group does not exist");
        };
        let chain = self
            .inner
            .store
            .sender_chain(account_id, group_id, &account.user_id)?
            .unwrap_or_else(|| SenderChain::new(crypto::random_key()));
        self.inner
            .store
            .save_sender_chain(account_id, group_id, &account.user_id, &chain)?;
        let distribution = SenderKeyDistribution {
            group_id: group_id.to_string(),
            group_name: group.name.clone(),
            sender_id: account.user_id.clone(),
            chain_key: base64::engine::general_purpose::STANDARD.encode(chain.to_bytes()),
            iteration: chain.iteration(),
            members: group.members.clone(),
            admins: group.admins.clone(),
            revoked: false,
        };
        for peer in group.peers(&account.user_id) {
            let item = QueuedItem {
                kind: "sender_key".into(),
                target: peer.user_id.clone(),
                message_id: String::new(),
                body: None,
                group_envelope: None,
                distribution: Some(distribution.clone()),
                receipt: None,
                attachments: Vec::new(),
                attachment_path: None,
                attachment_key: None,
            };
            self.inner.store.enqueue(
                account_id,
                "",
                &peer.user_id,
                &serde_json::to_string(&item)?,
            )?;
        }
        spawn_delivery(&self.inner, account_id, None);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Sending: one worker per account
// ---------------------------------------------------------------------------

/// Ask the peer for its prekey bundle, including a fresh one-time prekey.
///
/// This is the only network step before the first message. If the peer does not
/// answer, the invite's signed prekey is still enough to start X3DH, so the
/// message stays queued and nothing is invented.
async fn fetch_bundle(
    inner: &Arc<Inner>,
    account_id: &str,
    account: &MessengerAccount,
    contact: &MessengerContact,
) -> Result<Option<PrekeyBundle>> {
    // Deliberately no cache read. A cached bundle may name a one-time prekey the
    // peer has since spent, and reusing it makes them reject the message with
    // "the one-time prekey was already used". A fresh handshake per new session
    // is one extra round trip that only happens once per conversation.
    let invite = invite_bundle(contact);
    if contact.onion_address.is_empty() {
        return Ok(invite);
    }
    let Ok(socks) = peer::socks_port(inner.mesh.as_ref()).await else {
        return Ok(invite);
    };
    let secrets = inner.store.identity_secrets(account_id)?;
    let nonce = hex::encode(crypto::random_bytes(16));
    let signature = crypto::sign(
        &secrets.identity.signing(),
        format!(
            "handshake/v1/{nonce}:{}:{}",
            account.user_id, account.identity_pk
        )
        .as_bytes(),
    );
    let request = Request::Handshake {
        from_user_id: account.user_id.clone(),
        from_identity_pk: account.identity_pk.clone(),
        signature,
        nonce,
    };
    match peer::exchange(socks, &contact.onion_address, &request, DELIVERY_TIMEOUT).await {
        Ok(Reply::Bundle { bundle, .. }) => {
            // The invite's signature covers the long-lived prekey; a bundle that
            // disagrees with it is a substitution attempt.
            if bundle.identity_pk != contact.identity_pk
                || bundle.signed_prekey != contact.signed_prekey
            {
                bail!("the peer answered with a different prekey bundle than its invite");
            }
            // Used immediately for this one initiation and then dropped: a
            // one-time prekey it names is spent by the peer on receipt, so the
            // bundle must never outlive the message it was fetched for.
            Ok(Some(bundle))
        }
        // No answer is an offline peer, not a protocol error.
        _ => Ok(invite),
    }
}

fn invite_bundle(contact: &MessengerContact) -> Option<PrekeyBundle> {
    if contact.identity_pk.is_empty() || contact.signed_prekey.is_empty() {
        return None;
    }
    Some(PrekeyBundle {
        identity_pk: contact.identity_pk.clone(),
        signed_prekey: contact.signed_prekey.clone(),
        // The signature already checked out when the invite was imported.
        signed_prekey_sig: String::new(),
        one_time_prekey: None,
    })
}

/// Seal one payload for one peer, establishing the ratchet on first use.
///
/// A one-time prekey is spent by whoever receives it, so the bundle we initiate
/// with must be theirs, and it must be fetched fresh enough to still be in their
/// pool. When we have no cached bundle, the invite alone is used: it carries no
/// one-time prekey, which is a valid X3DH variant and cannot be "already used".
async fn seal_direct(
    inner: &Arc<Inner>,
    account_id: &str,
    account: &MessengerAccount,
    contact: &MessengerContact,
    payload: &[u8],
) -> Result<Envelope> {
    let secrets = inner.store.identity_secrets(account_id)?;
    let (mut state, preamble) = match inner.store.ratchet(account_id, &contact.user_id)? {
        Some(state) => (state, None),
        None => {
            // Inbound first: this direction is the peer's, and a bundle we
            // received from *them* names one of our own prekeys. Using that here
            // would ask their device to consume a prekey we already consumed —
            // exactly "the one-time prekey was already used".
            let Some(bundle) = fetch_bundle(inner, account_id, account, contact).await? else {
                bail!("there is no prekey material for this contact yet");
            };
            let initiation = crypto::initiate(&secrets.identity, &bundle, &contact.user_id)?;
            let remote = crypto::dh_public_key(&bundle.signed_prekey)?;
            let state = RatchetState::initiator(initiation.root, *remote.as_bytes());
            let preamble = X3dhPreamble {
                identity_pk: hex::encode(secrets.identity.dh_public()),
                ephemeral_pk: initiation.ephemeral_public,
                one_time_prekey: bundle.one_time_prekey.clone(),
                to_user_id: contact.user_id.clone(),
            };
            (state, Some(preamble))
        }
    };
    let (header, body) = state.encrypt(payload);
    inner
        .store
        .save_ratchet(account_id, &contact.user_id, &state)?;
    Ok(Envelope {
        v: PROTOCOL_VERSION,
        from_user_id: account.user_id.clone(),
        from_account_onion: account.onion_address.clone(),
        signature: crypto::sign(
            &secrets.identity.signing(),
            &format!(
                "envelope/v1/{}:{}:{}:{}:{}",
                account.user_id,
                contact.user_id,
                header.dh,
                header.previous_chain_length,
                header.message_number
            )
            .into_bytes(),
        ),
        header,
        ciphertext: base64::engine::general_purpose::STANDARD.encode(body),
        preamble,
    })
}

/// Start the delivery worker for an account unless one is already running.
/// `only_peer` narrows the pass to a single peer, so a reply does not restart
/// every other conversation's fan-out.
fn spawn_delivery(inner: &Arc<Inner>, account_id: &str, only_peer: Option<String>) {
    // A pass already running for this account will pick up anything queued while
    // it runs, so there is nothing to schedule: the queued flag is set instead of
    // starting a second, competing pass.
    if !inner
        .delivering
        .lock()
        .unwrap()
        .insert(account_id.to_string())
    {
        inner
            .reschedule
            .lock()
            .unwrap()
            .insert(account_id.to_string());
        return;
    }
    let inner = inner.clone();
    let account_id = account_id.to_string();
    tokio::spawn(async move {
        if let Err(error) = deliver_pass(&inner, &account_id, only_peer.as_deref()).await {
            inner
                .notice(format!("Messenger delivery failed: {error}"))
                .await;
        }
        inner.delivering.lock().unwrap().remove(&account_id);
        // Anything queued while the pass was running gets its own pass, but only
        // when a send actually arrived during the pass: an offline peer must not
        // reach this branch and spin.
        let asked = inner.reschedule.lock().unwrap().remove(&account_id);
        if asked && inner.store.outbox_len(&account_id).unwrap_or(0) > 0 {
            spawn_delivery(&inner, &account_id, None);
        }
    });
}

async fn deliver_pass(inner: &Arc<Inner>, account_id: &str, only_peer: Option<&str>) -> Result<()> {
    let Some(account) = inner.store.account(account_id)? else {
        return Ok(());
    };
    let items = inner.store.outbox(account_id)?;
    if items.is_empty() {
        return Ok(());
    }
    let socks = match peer::socks_port(inner.mesh.as_ref()).await {
        Ok(socks) => socks,
        Err(_) => {
            // Tor is not ready yet. The queue survives, but "the next pass"
            // only exists if something schedules it: a send, an onion coming
            // online, or this delayed retry. Without the retry, a message
            // sent while Tor was still bootstrapping sat Pending forever.
            let inner = inner.clone();
            let account_id = account_id.to_string();
            tokio::spawn(async move {
                tokio::time::sleep(RETRY_TOR_NOT_READY).await;
                if inner.store.outbox_len(&account_id).unwrap_or(0) > 0 {
                    spawn_delivery(&inner, &account_id, None);
                }
            });
            return Ok(());
        }
    };
    let mut any_offline = false;
    let mut queue: Vec<OutboxItem> = items
        .into_iter()
        .filter(|item| match only_peer {
            Some(peer_id) => item.peer_user_id == peer_id,
            None => true,
        })
        .collect();
    // Newest send request wins the ordering: the outbox is a queue, so it is
    // drained from the front.
    queue.reverse();
    while let Some(item) = queue.pop() {
        // Two consecutive plain messages to the same peer travel over one
        // circuit. Tor setup dominates the cost of a short message, so this is
        // what turns "type, wait, type, wait" into a single round trip.
        let sender_key = remembers_sender_key(&item);
        let mut batch = vec![item];
        if !sender_key {
            let onion = item_onion(inner, account_id, &batch[0])?;
            while !onion.is_empty() {
                let Some(next) = queue.last() else { break };
                // Only same-peer, same-onion, non-attachment plain messages:
                // a different peer needs its own circuit, and an attachment
                // streams its own chunks right after its control message.
                if next.peer_user_id != batch[0].peer_user_id
                    || remembers_sender_key(next)
                    || has_attachments(next)
                {
                    break;
                }
                if item_onion(inner, account_id, next)? != onion {
                    break;
                }
                batch.push(queue.pop().expect("peeked item must be present"));
            }
        }
        if batch.len() == 1 {
            let outcome = deliver_item(inner, &account, socks, &batch[0]).await;
            if apply_outcome(inner, account_id, &batch[0], outcome).await? {
                any_offline = true;
            }
            continue;
        }
        match deliver_batch(inner, &account, socks, &batch).await {
            Ok(outcomes) => {
                for (item, outcome) in batch.iter().zip(outcomes) {
                    if apply_outcome(inner, account_id, item, outcome).await? {
                        any_offline = true;
                    }
                }
            }
            Err(error) => {
                // A broken circuit invalidates the whole batch. Nothing was
                // confirmed, so every item stays queued and is retried rather
                // than being failed: the transport, not the peer, refused.
                for item in &batch {
                    if apply_outcome(inner, account_id, item, Ok(Outcome::Offline)).await? {
                        any_offline = true;
                    }
                }
                inner
                    .notice(format!("Messenger delivery paused: {error}"))
                    .await;
            }
        }
    }
    // An offline peer is never retried immediately. Without this, the worker
    // would spin on every queued message for an absent peer and open a fresh
    // circuit each time. `Pending` already means "waiting", so a delayed retry
    // is the correct behaviour rather than a compromise.
    if any_offline {
        let inner = inner.clone();
        let account_id = account_id.to_string();
        tokio::spawn(async move {
            tokio::time::sleep(RETRY_OFFLINE).await;
            if inner
                .store
                .outbox_len(&account_id)
                .map(|len| len > 0)
                .unwrap_or(false)
            {
                spawn_delivery(&inner, &account_id, None);
            }
        });
    }
    Ok(())
}

enum Outcome {
    Delivered { counted: bool },
    Offline,
}

/// Record what one delivery attempt means: dequeue and advance the row, keep it
/// queued, or fail it. Returns whether the peer looked offline, so the caller
/// can schedule the deferred retry.
///
/// Shared by the one-at-a-time and batched paths so both apply exactly the same
/// rules — a batch must not be a second, subtly different delivery policy.
async fn apply_outcome(
    inner: &Arc<Inner>,
    account_id: &str,
    item: &OutboxItem,
    outcome: Result<Outcome>,
) -> Result<bool> {
    match outcome {
        Ok(Outcome::Delivered { counted }) => {
            inner.store.dequeue(&item.id)?;
            if counted && let Some(record) = inner.store.message(&item.message_id)? {
                let total = record.total_members.max(1);
                let delivered = (record.delivered_to + 1).min(total);
                inner.store.set_delivery(&record.id, delivered, total)?;
                // `delivered_to` only ever moves forward because it is read back
                // from the row after this write, so a retry for one member
                // cannot lower another member's tally.
                let status = if delivered >= total {
                    MessageStatus::Delivered
                } else {
                    MessageStatus::Sent
                };
                if inner.store.raise_status(&record.id, status)?
                    && let Some(record) = inner.store.message(&record.id)?
                {
                    inner.emit_status(account_id, &record).await;
                }
            }
            mark_peer_online(inner, account_id, &item.peer_user_id).await;
            Ok(false)
        }
        // Offline: keep the item and try again on the next pass.
        Ok(Outcome::Offline) => {
            inner.store.note_attempt(&item.id)?;
            mark_peer_offline(inner, account_id, &item.peer_user_id).await;
            Ok(true)
        }
        // A protocol-level rejection would fail forever, so report it once.
        Err(error) => {
            inner.store.dequeue(&item.id)?;
            if let Some(record) = inner.store.message(&item.message_id)? {
                inner
                    .store
                    .update_status(&record.id, MessageStatus::Failed)?;
                if let Some(record) = inner.store.message(&record.id)? {
                    inner.emit_status(account_id, &record).await;
                }
            }
            inner
                .notice(format!(
                    "A messenger message could not be delivered: {error}"
                ))
                .await;
            Ok(false)
        }
    }
}

/// Seal one queued item into the request it should be sent as.
///
/// Split out from the send so a burst to the same peer can be sealed and then
/// pushed over one circuit, instead of paying a fresh circuit per message.
async fn seal_item(
    inner: &Arc<Inner>,
    account: &MessengerAccount,
    item: &OutboxItem,
) -> Result<Option<Request>> {
    let queued: QueuedItem =
        serde_json::from_str(&item.request_json).context("the queued message is unreadable")?;
    let account_id = account.account_id.as_str();
    match queued.kind.as_str() {
        "receipt" => Ok(Some(Request::Receipt {
            receipt: queued
                .receipt
                .clone()
                .context("a queued receipt carries no receipt")?,
        })),
        "group" => Ok(Some(Request::Group {
            envelope: Box::new(
                queued
                    .group_envelope
                    .clone()
                    .context("a queued group message carries no envelope")?,
            ),
        })),
        // Everything else is a 1:1 payload, and every 1:1 payload — a message, an
        // edit, or a sender key — is sealed inside the ratchet before it touches
        // the wire. Only the group envelope above is already encrypted, by its
        // sender chain.
        _ => {
            let contact = match inner.store.contact(account_id, &item.peer_user_id)? {
                Some(contact) => contact,
                // The contact is gone: nothing can be sealed for them, and the
                // caller drops the item rather than queueing it forever.
                None => return Ok(None),
            };
            // A sender key is a 1:1 payload whose *body* is the distribution; it
            // rides in the same envelope shape as chat so the receiver verifies
            // exactly one code path.
            let body = match queued.kind.as_str() {
                "sender_key" => distribution_body(
                    queued
                        .distribution
                        .as_ref()
                        .context("a sender key item carries no key")?,
                )?,
                _ => queued
                    .body
                    .clone()
                    .context("a queued message carries no body")?,
            };
            let envelope = seal_direct(
                inner,
                account_id,
                account,
                &contact,
                &serde_json::to_vec(&body)?,
            )
            .await?;
            // A preamble means we had no session and just built one. Deciding
            // the tag from the returned envelope, not from a store lookup made
            // before sealing, is what keeps the tag and the payload consistent:
            // the peer only establishes its responder half from `FirstMessage`,
            // so a first message tagged `Message` arrived with no session to
            // open and was refused.
            Ok(Some(if envelope.preamble.is_some() {
                Request::FirstMessage {
                    envelope: Box::new(envelope),
                }
            } else if queued.kind == "sender_key" {
                Request::SenderKey {
                    from_user_id: account.user_id.clone(),
                    envelope: Box::new(envelope),
                }
            } else {
                Request::Message {
                    envelope: Box::new(envelope),
                }
            }))
        }
    }
}

/// Where one queued item has to be sent.
fn item_onion(inner: &Arc<Inner>, account_id: &str, item: &OutboxItem) -> Result<String> {
    let queued: QueuedItem =
        serde_json::from_str(&item.request_json).context("the queued message is unreadable")?;
    if queued.kind == "group" {
        return group_onion(inner, account_id, &queued.target, &item.peer_user_id);
    }
    Ok(inner
        .store
        .contact(account_id, &item.peer_user_id)?
        .map(|contact| contact.onion_address)
        .unwrap_or_default())
}

/// Whether an item is a sender-key distribution.
///
/// It is always sent alone: it installs the group chain the *next* message is
/// sealed with, and a receiver has to have applied it before that message can be
/// opened.
fn remembers_sender_key(item: &OutboxItem) -> bool {
    serde_json::from_str::<QueuedItem>(&item.request_json)
        .map(|queued| queued.kind == "sender_key")
        .unwrap_or(false)
}

/// Whether an item has files to stream after its control message. Those chunks
/// need their own circuits, so the item is never batched.
fn has_attachments(item: &OutboxItem) -> bool {
    serde_json::from_str::<QueuedItem>(&item.request_json)
        .map(|queued| !queued.attachment_queue().is_empty())
        .unwrap_or(false)
}

/// Send several queued items to one peer over one circuit.
///
/// Every request is sealed first, in order, so the receiver opens them on one
/// chain exactly as if they had arrived one per circuit. The caller then applies
/// the same outcome rules it uses for a single message.
async fn deliver_batch(
    inner: &Arc<Inner>,
    account: &MessengerAccount,
    socks_port: u16,
    items: &[OutboxItem],
) -> Result<Vec<Result<Outcome>>> {
    let account_id = account.account_id.as_str();
    let onion = item_onion(inner, account_id, &items[0])?;
    if onion.is_empty() {
        return Ok(items.iter().map(|_| Ok(Outcome::Offline)).collect());
    }
    let mut requests = Vec::with_capacity(items.len());
    // Which item each request belongs to, so replies map back by index instead
    // of by pointer identity.
    let mut owners: Vec<usize> = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        match seal_item(inner, account, item).await {
            Ok(Some(request)) => {
                requests.push(request);
                owners.push(index);
            }
            // Nothing left to seal for a vanished contact: treated as handled so
            // the item is dequeued instead of retried forever.
            Ok(None) => {}
            Err(error) => {
                // Sealing failed for one item; the rest can still travel.
                return Ok(items
                    .iter()
                    .enumerate()
                    .map(|(candidate, _)| {
                        if candidate == index {
                            Err(anyhow::anyhow!("{error:#}"))
                        } else {
                            Ok(Outcome::Offline)
                        }
                    })
                    .collect());
            }
        }
    }
    if requests.is_empty() {
        return Ok(items
            .iter()
            .map(|_| Ok(Outcome::Delivered { counted: false }))
            .collect());
    }
    let replies = peer::exchange_batch(socks_port, &onion, &requests, DELIVERY_TIMEOUT).await?;
    let mut outcomes: Vec<Result<Outcome>> = Vec::with_capacity(items.len());
    for (index, _) in items.iter().enumerate() {
        // An item that produced no request has no reply of its own.
        let Some(reply_index) = owners.iter().position(|owner| *owner == index) else {
            outcomes.push(Ok(Outcome::Delivered { counted: false }));
            continue;
        };
        outcomes.push(match &replies[reply_index] {
            Reply::Accepted => {
                // Attachments follow their control message; batching never
                // includes them, so there is nothing to stream here.
                Ok(Outcome::Delivered { counted: true })
            }
            Reply::NotAContact { .. } => Ok(Outcome::Offline),
            Reply::Pong => Ok(Outcome::Delivered { counted: false }),
            Reply::Error { message } => {
                Err(anyhow::anyhow!("the peer rejected the message: {message}"))
            }
            _ => Ok(Outcome::Delivered { counted: true }),
        });
    }
    Ok(outcomes)
}

async fn deliver_item(
    inner: &Arc<Inner>,
    account: &MessengerAccount,
    socks_port: u16,
    item: &OutboxItem,
) -> Result<Outcome> {
    let account_id = account.account_id.as_str();
    let onion = item_onion(inner, account_id, item)?;
    if onion.is_empty() {
        return Ok(Outcome::Offline);
    }
    let Some(request) = seal_item(inner, account, item).await? else {
        // Nothing to seal any more: the contact this was queued for is gone.
        return Ok(Outcome::Delivered { counted: false });
    };
    let queued: QueuedItem =
        serde_json::from_str(&item.request_json).context("the queued message is unreadable")?;
    match peer::exchange(socks_port, &onion, &request, DELIVERY_TIMEOUT).await {
        Ok(Reply::Accepted) => {
            // Every attachment of the message follows its control envelope, in
            // the order the sender picked them.
            for part in queued.attachment_queue() {
                send_attachment(inner, account, item, socks_port, &onion, &part, &queued).await?;
            }
            Ok(Outcome::Delivered { counted: true })
        }
        // They have not accepted the request yet. Keep the message queued: the
        // text is exactly what the user expects to arrive after the Accept, and
        // deleting it here would silently lose a message they already sent.
        Ok(Reply::NotAContact { .. }) => Ok(Outcome::Offline),
        Ok(Reply::Pong) => Ok(Outcome::Delivered { counted: false }),
        Ok(Reply::Error { message }) => bail!("the peer rejected the message: {message}"),
        Ok(_) => Ok(Outcome::Delivered { counted: true }),
        Err(_) => Ok(Outcome::Offline),
    }
}

fn group_onion(
    inner: &Arc<Inner>,
    account_id: &str,
    group_id: &str,
    peer_user_id: &str,
) -> Result<String> {
    let Some(group) = inner.store.group(account_id, group_id)? else {
        bail!("a group message was queued for a group that no longer exists");
    };
    Ok(group
        .member(peer_user_id)
        .map(|member| member.onion.clone())
        .unwrap_or_default())
}

/// Stream the sealed chunks over fresh Tor circuits, one chunk per request.
///
/// A single reused stream would be faster but would also lose the whole
/// transfer on one circuit failure. Per-chunk requests cost a little cover
/// traffic and make progress durable: the receiver counts chunks, so a retry
/// resumes rather than restarts.
async fn send_attachment(
    inner: &Arc<Inner>,
    account: &MessengerAccount,
    item: &OutboxItem,
    socks_port: u16,
    onion: &str,
    part: &QueuedAttachment,
    queued: &QueuedItem,
) -> Result<()> {
    let path = part.path.as_str();
    let transfer_id = part_transfer_id(&item.message_id, part.index);
    let key: [u8; crypto::KEY_LEN] = hex::decode(&part.key)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .context("the attachment key is malformed")?;
    let meta: AttachmentMeta = inner
        .store
        .transfer_meta(&account.account_id, &transfer_id)?
        .and_then(|encoded| serde_json::from_str(&encoded).ok())
        .or_else(|| {
            queued
                .body
                .as_ref()
                .and_then(|body| body.parts().get(part.index as usize).cloned())
                .map(|part| part.meta)
        })
        .context("the attachment metadata is missing")?;
    let mut file = std::fs::File::open(path).with_context(|| format!("cannot open {path}"))?;
    let mut remaining = meta.size_bytes;
    for index in 0..meta.total_chunks {
        use std::io::Read as _;
        use std::io::Seek as _;
        let offset = u64::from(index) * u64::from(meta.chunk_size);
        file.seek(std::io::SeekFrom::Start(offset))?;
        let length = remaining.min(u64::from(meta.chunk_size)) as usize;
        let mut plain = vec![0u8; length];
        file.read_exact(&mut plain)?;
        remaining -= plain.len() as u64;
        let chunk_key = crypto::derive_key(
            &key,
            meta.sha256.as_bytes(),
            &format!("attachment/chunk/{index}"),
        );
        let sealed = crypto::seal(
            &chunk_key,
            &plain,
            format!(
                "attachment/v1/{}/{index}/{}",
                meta.sha256, meta.total_chunks
            )
            .as_bytes(),
        );
        let request = Request::Chunk {
            transfer_id: transfer_id.clone(),
            from_user_id: account.user_id.clone(),
            index,
            data: base64::engine::general_purpose::STANDARD.encode(sealed),
        };
        let received = match peer::exchange(socks_port, onion, &request, DELIVERY_TIMEOUT).await {
            Ok(Reply::ChunkAccepted { received, .. }) => received,
            Ok(Reply::Error { message }) => bail!("the peer rejected a chunk: {message}"),
            Ok(_) => index + 1,
            Err(_) => {
                // One lost circuit must not fail the whole file: report what the
                // peer confirmed and leave the transfer resumable.
                let received = index;
                inner
                    .store
                    .set_transfer_progress(&transfer_id, received, "interrupted")?;
                inner
                    .emit_media(
                        &account.account_id,
                        &item.message_id,
                        &transfer_id,
                        "outgoing",
                        &meta.file_name,
                        received,
                        meta.total_chunks,
                        "interrupted",
                    )
                    .await;
                return Ok(());
            }
        };
        inner
            .store
            .set_transfer_progress(&transfer_id, received, "active")?;
        inner
            .emit_media(
                &account.account_id,
                &item.message_id,
                &transfer_id,
                "outgoing",
                &meta.file_name,
                received,
                meta.total_chunks,
                "active",
            )
            .await;
    }
    inner
        .store
        .set_transfer_progress(&transfer_id, meta.total_chunks, "complete")?;
    inner
        .emit_media(
            &account.account_id,
            &item.message_id,
            &transfer_id,
            "outgoing",
            &meta.file_name,
            meta.total_chunks,
            meta.total_chunks,
            "complete",
        )
        .await;
    Ok(())
}

/// Presence is a ping, not a subscription: a peer counts as online for this
/// device only after its onion answered. Transitions are what the UI shows, so
/// a flapping peer does not spam the transcript.
async fn mark_peer_online(inner: &Arc<Inner>, account_id: &str, peer_user_id: &str) {
    let inserted = inner
        .presence
        .lock()
        .unwrap()
        .insert((account_id.to_string(), peer_user_id.to_string()));
    if inserted {
        inner
            .emit(Event::MessengerPeerOnline {
                account_id: account_id.to_string(),
                user_id: peer_user_id.to_string(),
            })
            .await;
    }
}

async fn mark_peer_offline(inner: &Arc<Inner>, account_id: &str, peer_user_id: &str) {
    let removed = inner
        .presence
        .lock()
        .unwrap()
        .remove(&(account_id.to_string(), peer_user_id.to_string()));
    if removed {
        inner
            .emit(Event::MessengerPeerOffline {
                account_id: account_id.to_string(),
                user_id: peer_user_id.to_string(),
            })
            .await;
    }
}

/// A light heartbeat: ping every known contact, report transitions, and flush
/// the queue the moment somebody comes back.
///
/// The same tick re-sends outgoing contact requests. A request is one Tor
/// round trip, and the peer can be unreachable for reasons that have nothing to
/// do with consent: Tor still bootstrapping on their side, the phone asleep, a
/// circuit that died mid-handshake. Sending it exactly once means the other
/// device never learns we asked, never shows the approval, and never writes us
/// as a contact — which is why every message we queue behind it sits `Pending`
/// forever. Retrying here is what turns "waiting for them to accept" into an
/// actual acceptance.
async fn retry_pending_requests(inner: &Arc<Inner>, account_id: &str, account: &MessengerAccount) {
    let Ok(requests) = inner.store.contact_requests(account_id) else {
        return;
    };
    let own_invite = invite_payload(account).to_string();
    for request in requests.iter().filter(|row| row.direction == "outgoing") {
        // The stored invite is the only thing that names their identity keys, so
        // a row that no longer parses is skipped rather than half-trusted.
        let Ok(contact) = parse_invite(&request.invite_json) else {
            continue;
        };
        if contact.user_id != request.user_id {
            continue;
        }
        match send_contact_request_with(
            inner,
            account,
            &contact,
            &own_invite,
            REQUEST_RETRY_TIMEOUT,
        )
        .await
        {
            ContactOutcome::Accepted => {
                let _ = inner.store.upsert_contact(account_id, &contact);
                let _ = inner
                    .store
                    .remove_contact_request(account_id, &contact.user_id);
                inner.emit_conversations(account_id).await;
                inner.emit_contact_requests(account_id).await;
                inner
                    .notice(format!(
                        "{} accepted you. You can chat now.",
                        contact.display_name
                    ))
                    .await;
                // Everything typed while the request was unanswered is still
                // queued: deliver it now instead of waiting for the retry timer.
                spawn_delivery(inner, account_id, Some(contact.user_id.clone()));
            }
            // Still unanswered, or still unreachable. The row stays, and the
            // next tick tries again.
            ContactOutcome::Pending | ContactOutcome::Unreachable(_) => {}
        }
    }
}

fn spawn_presence(inner: &Arc<Inner>, account_id: &str, mut stop: watch::Receiver<bool>) {
    let inner = inner.clone();
    let account_id = account_id.to_string();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = stop.changed() => break,
                _ = tokio::time::sleep(PRESENCE_INTERVAL) => {}
            }
            let account_id = account_id.clone();
            let inner = inner.clone();
            // Presence work is strictly best-effort.
            // An account that was deleted between ticks simply has nobody to
            // ping, so the heartbeat goes quiet instead of failing.
            let Ok(Some(account)) = inner.store.account(&account_id) else {
                continue;
            };
            let Ok(socks) = peer::socks_port(inner.mesh.as_ref()).await else {
                continue;
            };
            let contacts = inner.store.contacts(&account_id).unwrap_or_default();
            // One circuit per peer, not per contact: contacts on different
            // onions cannot share a stream, but everything else about the tick
            // is latency the user sees. Grouping also means a peer that is
            // reachable is noticed in one round trip instead of queueing behind
            // every unreachable peer ahead of it in the list.
            let reachable: Vec<_> = contacts
                .into_iter()
                .filter(|contact| !contact.onion_address.is_empty())
                .collect();
            for contact in reachable {
                match peer::exchange(
                    socks,
                    &contact.onion_address,
                    &Request::Ping,
                    PRESENCE_TIMEOUT,
                )
                .await
                {
                    Ok(Reply::Pong) => {
                        mark_peer_online(&inner, &account_id, &contact.user_id).await;
                    }
                    _ => mark_peer_offline(&inner, &account_id, &contact.user_id).await,
                }
            }
            // After the pings, so a peer that just came back is warm.
            retry_pending_requests(&inner, &account_id, &account).await;
        }
    });
}

// ---------------------------------------------------------------------------
// Inbound
// ---------------------------------------------------------------------------

/// One connection on the account's onion: decode one request, answer it.
///
/// Every inbound path is authenticated before it changes state. A peer may be
/// anonymous to us until it proves it holds the identity its invite names, and
/// an envelope from somebody we never added is dropped, not stored: an onion is
/// public, so "anyone can knock" has to be a safe default.
async fn serve_connection(
    inner: Arc<Inner>,
    account_id: String,
    mut stream: tokio::net::TcpStream,
) {
    // Answer frames until the peer stops talking.
    //
    // Reading exactly one frame and closing is what produced "early eof" and
    // "Broken pipe (os error 32)" on the sender: a batched send puts several
    // requests on one stream, and the receiver hung up after the first reply, so
    // the next write had nowhere to go. The protocol is framed and ordered, which
    // only works if the connection outlives a single request.
    loop {
        let frame = match tokio::time::timeout(
            // A peer that connects and then says nothing must not hold a slot.
            Duration::from_secs(120),
            peer::read_frame(&mut stream),
        )
        .await
        {
            Ok(Ok(frame)) => frame,
            // Timeout, clean close, or a truncated frame: nothing left to answer.
            _ => return,
        };
        let Ok(text) = std::str::from_utf8(&frame) else {
            return;
        };
        let reply = match super::wire::decode_request(text) {
            Ok(request) => handle_request(&inner, &account_id, request).await,
            Err(error) => Reply::Error {
                message: error.to_string(),
            },
        };
        let Ok(encoded) = serde_json::to_vec(&reply) else {
            return;
        };
        // A failed write means the peer is gone; stop reading for it too.
        if peer::write_frame(&mut stream, &encoded).await.is_err() {
            return;
        }
    }
}

async fn handle_request(inner: &Arc<Inner>, account_id: &str, request: Request) -> Reply {
    match handle_request_inner(inner, account_id, request).await {
        Ok(reply) => reply,
        // An envelope from somebody this device has not accepted yet is not a
        // protocol failure. Answering with a distinct reply is what lets the
        // sender keep its message queued instead of marking it Failed, so the
        // text is still there the moment the request is accepted.
        Err(error) if error.to_string().contains("not a contact of this account") => {
            let user_id = inner
                .store
                .account(account_id)
                .ok()
                .flatten()
                .map(|account| account.user_id)
                .unwrap_or_default();
            Reply::NotAContact { user_id }
        }
        Err(error) => Reply::Error {
            message: error.to_string(),
        },
    }
}

async fn handle_request_inner(
    inner: &Arc<Inner>,
    account_id: &str,
    request: Request,
) -> Result<Reply> {
    match request {
        // ---------------------------------------------------------------------
        // Contact requests: the only way a stranger becomes a contact.
        //
        // Scanning a QR is not consent on the other side. The scanner sends its
        // own signed invite here, this device stores it as a *pending request*
        // and shows it in the Chat tab, and the contact row is written only
        // when the user accepts. That is what makes the introduction mutual instead of
        // one device silently deciding for both.
        // ---------------------------------------------------------------------
        Request::ContactRequest { invite } => {
            let contact = parse_invite(&invite)
                .context("the contact request does not carry a valid invite")?;
            let account = inner
                .store
                .account(account_id)?
                .context("the account does not exist")?;
            if contact.user_id == account.user_id {
                bail!("a contact request cannot name this account");
            }
            // Already a contact: nothing to ask. Answering with our invite lets
            // the sender complete its own side without a second round trip.
            if inner.store.contact(account_id, &contact.user_id)?.is_some() {
                return Ok(Reply::ContactAnswer {
                    user_id: account.user_id.clone(),
                    accepted: true,
                    invite: invite_payload(&account).to_string(),
                });
            }
            inner
                .store
                .upsert_contact_request(account_id, &contact, &invite, "incoming")?;
            // Also land a note in the conversation itself. The request banner
            // only exists on the Chat home screen, so on a phone that is inside
            // another chat — or on another tab — the invitation would arrive
            // with nothing to see. A system message is part of the transcript,
            // counts as unread, and survives a restart.
            //
            // `None` means this is a retry of a request already on disk, which
            // is the normal case while the person decides: the sender re-asks
            // every heartbeat. A retry must not re-notify or duplicate the note.
            if let Some(note) = note_contact_request(inner, account_id, &contact).await? {
                // Emitted as an ordinary incoming message so an open transcript
                // appends it there and then, without a refresh round trip.
                inner
                    .emit(Event::MessengerMessageReceived {
                        account_id: account_id.to_string(),
                        message: serde_json::to_value(MessageView::from_record(&note))
                            .unwrap_or_default(),
                    })
                    .await;
                inner.emit_conversations(account_id).await;
                inner.emit_contact_requests(account_id).await;
                inner
                    .notice(format!(
                        "{} wants to chat. Accept the request in the Chat tab.",
                        contact.display_name
                    ))
                    .await;
            }
            // Pending until the user answers; the sender retries and the next
            // request is answered with the decision.
            Ok(Reply::ContactPending {
                user_id: account.user_id.clone(),
            })
        }
        Request::ContactResponse {
            user_id,
            accepted,
            invite,
        } => {
            let account = inner
                .store
                .account(account_id)?
                .context("the account does not exist")?;
            let contact = parse_invite(&invite)
                .context("the contact response does not carry a valid invite")?;
            if contact.user_id != user_id {
                bail!("the contact response names a different identity than its invite");
            }
            if accepted {
                inner.store.upsert_contact(account_id, &contact)?;
                inner.store.remove_contact_request(account_id, &user_id)?;
                inner
                    .store
                    .remove_contact_request_note(account_id, &user_id)?;
                inner.emit_conversations(account_id).await;
                inner.emit_contact_requests(account_id).await;
                inner
                    .notice(format!("{} accepted your request.", contact.display_name))
                    .await;
                // Everything this device typed while waiting is still queued:
                // the peer rejected it with `NotAContact`, which is retried on a
                // two-minute timer. Flush it now — the acceptance just arrived,
                // so this is the moment the queue can succeed.
                let _ = account;
                spawn_delivery(inner, account_id, Some(user_id.clone()));
            } else {
                inner.store.remove_contact_request(account_id, &user_id)?;
                inner
                    .store
                    .remove_contact_request_note(account_id, &user_id)?;
                inner.emit_contact_requests(account_id).await;
                inner
                    .notice(format!("{} declined your request.", contact.display_name))
                    .await;
            }
            Ok(Reply::Accepted)
        }
        // ---------------------------------------------------------------------
        // Prekey bundle: the only request an unknown peer may make. It hands out
        // a signed prekey and one one-time prekey, and nothing else.
        // ---------------------------------------------------------------------
        Request::Handshake { .. } => {
            let account = inner
                .store
                .account(account_id)?
                .context("the account does not exist")?;
            let one_time = inner
                .store
                .take_one_time_prekey(account_id)
                .ok()
                .flatten()
                .map(|prekey| prekey.public);
            // Top the pool back up so the next invitee can also have one.
            let _ = inner
                .store
                .ensure_one_time_prekeys(account_id, ONE_TIME_PREKEY_TARGET);
            Ok(Reply::Bundle {
                user_id: account.user_id.clone(),
                display_name: account.display_name.clone(),
                bundle: PrekeyBundle {
                    identity_pk: account.identity_pk.clone(),
                    signed_prekey: account.signed_prekey.clone(),
                    signed_prekey_sig: account.prekey_signature.clone(),
                    one_time_prekey: one_time,
                },
            })
        }
        Request::Ping => Ok(Reply::Pong),

        // ------------------------------ 1:1 ------------------------------
        // `FirstMessage` and `Message` differ only in whether a preamble is
        // present; `open_direct` handles both. The separate tag exists so a
        // receiver can distinguish "start a session" from "continue one"
        // without guessing, and so a missing preamble is a protocol error rather
        // than a silent decryption failure.
        Request::FirstMessage { envelope } => {
            if envelope.preamble.is_none() {
                bail!("the first message of a conversation must carry its preamble");
            }
            let (contact, body) = open_direct(inner, account_id, *envelope).await?;
            accept_direct(inner, account_id, &contact.user_id, body).await?;
            Ok(Reply::Accepted)
        }
        Request::Message { envelope } => {
            let (contact, body) = open_direct(inner, account_id, *envelope).await?;
            accept_direct(inner, account_id, &contact.user_id, body).await?;
            Ok(Reply::Accepted)
        }

        Request::SenderKey {
            from_user_id,
            envelope,
        } => {
            // The distribution lives inside the envelope, so it is authenticated
            // by the ratchet before any of it is trusted.
            let (_, body) = open_direct(inner, account_id, *envelope).await?;
            let distribution = sealed_distribution(&body)?;
            if distribution.sender_id != from_user_id {
                bail!("a sender key must be signed by the member it belongs to");
            }
            if distribution.members.len() > MAX_GROUP_MEMBERS {
                bail!("a group is limited to {MAX_GROUP_MEMBERS} members");
            }
            let account = inner
                .store
                .account(account_id)?
                .context("the account does not exist")?;
            let mut group = inner
                .store
                .group(account_id, &distribution.group_id)?
                .unwrap_or(GroupRecord {
                    group_id: distribution.group_id.clone(),
                    account_id: account_id.to_string(),
                    name: if distribution.group_name.is_empty() {
                        "Group".into()
                    } else {
                        distribution.group_name.clone()
                    },
                    created_by: distribution.sender_id.clone(),
                    created_at: chrono::Utc::now().timestamp_millis(),
                    members: Vec::new(),
                    admins: Vec::new(),
                    revoked: false,
                });
            if group.member(&account.user_id).is_none()
                && !distribution
                    .members
                    .iter()
                    .any(|member| member.user_id == account.user_id)
            {
                bail!("the sender key is for a group this account is not in");
            }
            group.members = distribution.members.clone();
            group.admins = distribution.admins.clone();
            if !distribution.group_name.is_empty() {
                group.name = distribution.group_name.clone();
            }
            inner.store.save_group(&group)?;
            inner
                .store
                .group_conversation(account_id, &distribution.group_id)?;
            let chain = SenderChain::from_bytes(
                base64::engine::general_purpose::STANDARD
                    .decode(&distribution.chain_key)
                    .context("the sender chain key is not valid base64")?,
            )?;
            // A distribution always starts a fresh chain: a later one replaces
            // the old chain rather than continuing it, so a rotation after a
            // member is removed cannot be replayed against older keys.
            inner.store.save_sender_chain(
                account_id,
                &distribution.group_id,
                &distribution.sender_id,
                &chain,
            )?;
            inner.emit_groups(account_id).await;
            inner.emit_group(account_id, &group).await;
            Ok(Reply::Accepted)
        }

        // ----------------------------- group -----------------------------
        Request::Group { envelope } => {
            let body = open_group(inner, account_id, *envelope).await?;
            accept_group(inner, account_id, body).await?;
            Ok(Reply::Accepted)
        }

        // ---------------------------- chunk ------------------------------
        Request::Chunk {
            transfer_id,
            from_user_id,
            index,
            data,
        } => {
            let sealed = base64::engine::general_purpose::STANDARD
                .decode(data)
                .context("a chunk is not valid base64")?;
            let (received, total) = accept_chunk(
                inner,
                account_id,
                &transfer_id,
                index,
                &sealed,
                (!from_user_id.is_empty()).then_some(from_user_id.as_str()),
            )
            .await?;
            Ok(Reply::ChunkAccepted {
                transfer_id,
                received,
                total,
            })
        }

        // --------------------------- receipt -----------------------------
        Request::Receipt { receipt } => {
            let status = match receipt.status.as_str() {
                "read" => MessageStatus::Read,
                "delivered" => MessageStatus::Delivered,
                "failed" => MessageStatus::Failed,
                _ => bail!("unknown receipt status"),
            };
            let Some(record) = inner.store.message(&receipt.message_id)? else {
                return Ok(Reply::Accepted);
            };
            // Receipts are only honoured for our own outgoing rows.
            if record.account_id != account_id || record.direction != "outgoing" {
                return Ok(Reply::Accepted);
            }
            if inner.store.raise_status(&record.id, status)?
                && let Some(record) = inner.store.message(&record.id)?
            {
                inner.emit_status(account_id, &record).await;
            }
            Ok(Reply::Accepted)
        }
    }
}

/// Open a 1:1 envelope, returning the contact it came from and the body.
///
/// Order matters: the sender must already be known (a contact, or a member of
/// one of our groups), the preamble must name *this* account, the signature
/// must verify against the sender's identity, and only then does the ratchet
/// run. A stranger's onion knock therefore changes no state at all.
async fn open_direct(
    inner: &Arc<Inner>,
    account_id: &str,
    envelope: Envelope,
) -> Result<(MessengerContact, MessageBody)> {
    if envelope.v != PROTOCOL_VERSION {
        bail!("the envelope was written by a different messenger version");
    }
    let account = inner
        .store
        .account(account_id)?
        .context("the account does not exist")?;
    let contact = match inner.store.contact(account_id, &envelope.from_user_id)? {
        Some(contact) => contact,
        None => group_contact(inner, account_id, &envelope.from_user_id)?
            .context("the sender is not a contact of this account")?,
    };
    // The signature is Ed25519, so it verifies against the sender's *signing*
    // key — which the invite makes equal to `user_id` — not against the X25519
    // key X3DH uses. Mixing the two would reject every honest message.
    let signing = crypto::verifying_key(&contact.user_id)
        .context("the sender's user id is not a signing key")?;
    crypto::verify(
        &signing,
        &envelope.signed_bytes(&account.user_id),
        &envelope.signature,
    )
    .context("the envelope's signature does not match its sender")?;
    if let Some(preamble) = &envelope.preamble {
        if preamble.to_user_id != account.user_id {
            bail!("the envelope is addressed to a different account");
        }
        establish_session(inner, account_id, &contact, preamble)?;
    }
    // Read on the direction the peer opened, never on the one we opened: the
    // two are separate keys, and using ours would fail authentication on every
    // message once both sides had started.
    let inbound = inbound_session_key(&contact.user_id);
    let mut state = inner
        .store
        .ratchet(account_id, &inbound)?
        .context("no session exists for this sender yet")?;
    let plaintext = state.decrypt(&envelope.header, &decode_base64(&envelope.ciphertext)?)?;
    inner.store.save_ratchet(account_id, &inbound, &state)?;
    let body = serde_json::from_slice(&plaintext).context("the message body is malformed")?;
    Ok((contact, body))
}

/// A group member we do not have as a contact yet. Their identity comes from
/// the group's member list, which an admin distributed over 1:1.
fn group_contact(
    inner: &Arc<Inner>,
    account_id: &str,
    user_id: &str,
) -> Result<Option<MessengerContact>> {
    for group in inner.store.groups(account_id)? {
        if let Some(member) = group.member(user_id) {
            if member.identity_pk.is_empty() {
                continue;
            }
            return Ok(Some(MessengerContact {
                user_id: member.user_id.clone(),
                display_name: member.display_name.clone(),
                identity_pk: member.identity_pk.clone(),
                signed_prekey: member.signed_prekey.clone(),
                onion_address: member.onion.clone(),
                verified: false,
            }));
        }
    }
    Ok(None)
}

/// Storage key for the direction the *peer* opened.
///
/// A 1:1 conversation has two independent one-way ratchets, one per direction.
/// Each side always sends on the chain it started, and always reads on the
/// chain the peer started. Two keys per peer are what keeps those apart.
///
/// The reason this is not a single shared session: once two accounts have each
/// other as a contact, both composers are enabled, so both may seal a first
/// message before either has read one. Each seals under its own X3DH root, and
/// two roots are two different keys. With one session per peer, the second side
/// to receive would decrypt the peer's message with a root that peer never
/// derived, and report exactly "sealed data failed authentication".
fn inbound_session_key(user_id: &str) -> String {
    format!("{user_id}|in")
}

/// The responder half of X3DH. It consumes one of our one-time prekeys exactly
/// once: a replayed preamble finds no key and stays undecryptable.
fn establish_session(
    inner: &Arc<Inner>,
    account_id: &str,
    contact: &MessengerContact,
    preamble: &X3dhPreamble,
) -> Result<()> {
    // Only the inbound direction is decided here. A ratchet we opened ourselves
    // must not stop the peer's direction from being established, because the
    // two are separate keys — that early return was why a crossed start left
    // every following message unreadable.
    let inbound = inbound_session_key(&contact.user_id);
    if inner.store.ratchet(account_id, &inbound)?.is_some() {
        return Ok(());
    }
    let secrets = inner.store.identity_secrets(account_id)?;
    if preamble.identity_pk != contact.identity_pk {
        bail!("the preamble names a different identity than the contact");
    }
    let one_time = match preamble.one_time_prekey.as_deref() {
        Some(public) => {
            let consumed = inner
                .store
                .consume_one_time_prekey(account_id, public)?
                .context("the one-time prekey was already used")?;
            let bytes = hex::decode(&consumed.secret)
                .map_err(|_| anyhow::anyhow!("a stored prekey is malformed"))?;
            Some(x25519_dalek::StaticSecret::from(
                <[u8; 32]>::try_from(bytes)
                    .map_err(|_| anyhow::anyhow!("a stored prekey is malformed"))?,
            ))
        }
        None => None,
    };
    let root = crypto::respond(
        &secrets.identity,
        &secrets.signed_prekey,
        one_time.as_ref(),
        &preamble.identity_pk,
        &preamble.ephemeral_pk,
        &contact.user_id,
    )?;
    inner.store.save_ratchet(
        account_id,
        &inbound,
        &RatchetState::responder(root, &secrets.signed_prekey),
    )?;
    Ok(())
}

fn decode_base64(value: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(value)
        .context("the payload is not valid base64")
}

/// Put "X wants to chat" into the conversation transcript.
///
/// The banner above the conversation list is the only surface a contact request
/// has, and it exists on the Chat home screen of the account that received it.
/// A phone sitting inside another chat, or on any other tab, sees nothing at
/// all — which is exactly how a request appears to have been lost. A system
/// row is visible wherever the conversation is, bumps the unread count, and is
/// still there after a restart.
///
/// The id is derived from the requester, so the retry that follows a lost
/// circuit replaces the note instead of stacking a second copy of it.
async fn note_contact_request(
    inner: &Arc<Inner>,
    account_id: &str,
    contact: &MessengerContact,
) -> Result<Option<MessageRecord>> {
    let conversation = inner
        .store
        .direct_conversation(account_id, &contact.user_id)?;
    let id = format!("request:{account_id}:{}", contact.user_id);
    // A retry of a request that already landed must not stack a second note.
    if inner.store.message(&id)?.is_some() {
        return Ok(None);
    }
    // The name is attacker-controlled text, so it is passed through as data and
    // the UI renders it as markdown source, never as an instruction.
    let text = format!(
        "**{}** wants to chat. Open the accept request above the conversation list.",
        contact.display_name
    );
    inner.store.insert_message(&NewMessage {
        id: id.clone(),
        conversation_id: conversation,
        account_id: account_id.to_string(),
        direction: "incoming".into(),
        from_user_id: contact.user_id.clone(),
        to_user_id: String::new(),
        ts: chrono::Utc::now().timestamp_millis(),
        kind: MessageKind::System.as_str().to_string(),
        text: Some(text),
        reply_to: None,
        media_json: None,
        status: MessageStatus::Delivered,
        ratchet_header: None,
        ciphertext: None,
        signature: None,
        delivered_to: 0,
        total_members: 0,
    })?;
    inner.store.message(&id)
}

/// Pull the sender-key distribution out of an already-opened 1:1 body.
///
/// A body that is not a distribution is refused here rather than silently
/// treated as a chat message, so a `SenderKey` request can never smuggle
/// something else past the group-membership checks.
fn sealed_distribution(body: &MessageBody) -> Result<SenderKeyDistribution> {
    if body.kind != MessageKind::System || body.text.is_some() {
        bail!("a sender key request did not carry a sender key distribution");
    }
    let encoded = body
        .attachment_key
        .as_deref()
        .context("a sender key request carries no distribution")?;
    serde_json::from_slice(&decode_base64(encoded)?)
        .context("the sender key distribution is malformed")
}

/// Wrap a sender-key distribution as an ordinary 1:1 body.
///
/// Reusing the chat body shape keeps exactly one verification path on the
/// receive side: the same signature check, the same ratchet step, the same
/// "sender must already be known" rule.
fn distribution_body(distribution: &SenderKeyDistribution) -> Result<MessageBody> {
    Ok(MessageBody {
        message_id: crypto::random_id(),
        kind: MessageKind::System,
        text: None,
        attachments: Vec::new(),
        attachment: None,
        attachment_key: Some(
            base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(distribution)?),
        ),
        reply_to: None,
        edit_of: None,
        group_id: Some(distribution.group_id.clone()),
        sender_id: Some(distribution.sender_id.clone()),
        sender_name: None,
        iteration: Some(distribution.iteration),
        sent_at: chrono::Utc::now().timestamp_millis(),
    })
}

/// Open a group envelope with the sender's chain.
async fn open_group(
    inner: &Arc<Inner>,
    account_id: &str,
    envelope: GroupEnvelope,
) -> Result<MessageBody> {
    if envelope.v != PROTOCOL_VERSION {
        bail!("the group envelope was written by a different messenger version");
    }
    let Some(group) = inner.store.group(account_id, &envelope.group_id)? else {
        bail!("a message arrived for a group this account is not in");
    };
    let Some(member) = group.member(&envelope.sender_id).cloned() else {
        bail!("the sender is not a member of this group");
    };
    let signing = crypto::verifying_key(&member.user_id)
        .context("the member's user id is not a signing key")?;
    crypto::verify(&signing, &envelope.signed_bytes(), &envelope.signature)
        .context("the group envelope's signature does not match its sender")?;
    let mut chain = inner
        .store
        .sender_chain(account_id, &envelope.group_id, &envelope.sender_id)?
        .context("no sender chain exists for this member yet")?;
    let key = chain.advance_to(envelope.iteration)?;
    inner
        .store
        .save_sender_chain(account_id, &envelope.group_id, &envelope.sender_id, &chain)?;
    let plaintext = crypto::open(
        &key,
        &decode_base64(&envelope.ciphertext)?,
        envelope.group_id.as_bytes(),
    )?;
    let mut body: MessageBody =
        serde_json::from_slice(&plaintext).context("the group message body is malformed")?;
    body.sender_id.get_or_insert(envelope.sender_id.clone());
    if body.sender_name.is_none() {
        body.sender_name = envelope
            .sender_name
            .clone()
            .or_else(|| Some(member.display_name.clone()));
    }
    Ok(body)
}

/// An attachment key travels as hex in the body; accept both encodings so a
/// future client cannot silently break delivery.
fn decode_attachment_key(value: &str) -> Option<[u8; crypto::KEY_LEN]> {
    hex::decode(value)
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .or_else(|| decode_base64(value).ok().and_then(|b| b.try_into().ok()))
}

/// Register an incoming attachment so its chunks have somewhere to land.
fn register_transfer(
    inner: &Arc<Inner>,
    account_id: &str,
    sender: &str,
    message_id: &str,
    index: u32,
    meta: &AttachmentMeta,
    key_value: &str,
) -> Result<()> {
    let key =
        decode_attachment_key(key_value).context("the attachment key has the wrong length")?;
    // Each part gets its own transfer id, derived from the message and the
    // part's position, so chunks of a second file never land in the first.
    let transfer_id = part_transfer_id(message_id, index);
    inner.store.start_transfer(
        &transfer_id,
        account_id,
        sender,
        "incoming",
        message_id,
        "",
        &serde_json::to_string(meta)?,
        &key,
    )?;
    inner.transfers.lock().unwrap().insert(
        transfer_id,
        IncomingTransfer {
            account_id: account_id.to_string(),
            peer_user_id: sender.to_string(),
            message_id: message_id.to_string(),
            meta: meta.clone(),
            incoming: media::Incoming::new(
                meta.sha256.clone(),
                meta.size_bytes,
                meta.total_chunks,
                meta.chunk_size,
                key,
            ),
        },
    );
    Ok(())
}

/// Store an accepted 1:1 body and tell the UI.
async fn accept_direct(
    inner: &Arc<Inner>,
    account_id: &str,
    sender: &str,
    body: MessageBody,
) -> Result<()> {
    if let Some(target) = body.edit_of.clone() {
        // An edit is applied in place, and only by the message's own author.
        if let Some(existing) = inner.store.message(&target)?
            && existing.account_id == account_id
            && existing.from_user_id == sender
            && existing.direction == "incoming"
        {
            let new_text = body.text.clone().unwrap_or_default();
            if let Some(record) = inner.store.edit_message(account_id, &target, &new_text)? {
                inner
                    .emit(Event::MessengerMessageEdited {
                        account_id: account_id.to_string(),
                        message: serde_json::to_value(MessageView::from_record(&record))
                            .unwrap_or_default(),
                    })
                    .await;
            }
        }
        return Ok(());
    }
    let conversation = inner.store.direct_conversation(account_id, sender)?;
    inner.store.insert_message(&NewMessage {
        id: body.message_id.clone(),
        conversation_id: conversation,
        account_id: account_id.to_string(),
        direction: "incoming".into(),
        from_user_id: sender.to_string(),
        to_user_id: String::new(),
        ts: body.sent_at,
        kind: body.kind.as_str().to_string(),
        text: body.text.clone(),
        reply_to: body.reply_to.clone(),
        media_json: incoming_media_json(&body)?,
        status: MessageStatus::Delivered,
        ratchet_header: None,
        ciphertext: None,
        signature: None,
        delivered_to: 0,
        total_members: 0,
    })?;
    for (index, part) in body.parts().iter().enumerate() {
        register_transfer(
            inner,
            account_id,
            sender,
            &body.message_id,
            index as u32,
            &part.meta,
            &part.key,
        )?;
    }
    if let Some(record) = inner.store.message(&body.message_id)? {
        inner
            .emit(Event::MessengerMessageReceived {
                account_id: account_id.to_string(),
                message: serde_json::to_value(MessageView::from_record(&record))
                    .unwrap_or_default(),
            })
            .await;
    }
    inner.emit_conversations(account_id).await;
    Ok(())
}

/// Group bodies arrive already attributed by their sender chain.
async fn accept_group(inner: &Arc<Inner>, account_id: &str, body: MessageBody) -> Result<()> {
    let group_id = body
        .group_id
        .clone()
        .context("a group message body names no group")?;
    let sender = body
        .sender_id
        .clone()
        .context("a group message names no sender")?;
    if let Some(target) = body.edit_of.clone() {
        if let Some(existing) = inner.store.message(&target)?
            && existing.account_id == account_id
            && existing.from_user_id == sender
            && existing.direction == "incoming"
        {
            let new_text = body.text.clone().unwrap_or_default();
            if let Some(record) = inner.store.edit_message(account_id, &target, &new_text)? {
                inner
                    .emit(Event::MessengerMessageEdited {
                        account_id: account_id.to_string(),
                        message: serde_json::to_value(MessageView::from_record(&record))
                            .unwrap_or_default(),
                    })
                    .await;
            }
        }
        return Ok(());
    }
    let conversation = inner.store.group_conversation(account_id, &group_id)?;
    inner.store.insert_message(&NewMessage {
        id: body.message_id.clone(),
        conversation_id: conversation,
        account_id: account_id.to_string(),
        direction: "incoming".into(),
        from_user_id: sender.clone(),
        to_user_id: String::new(),
        ts: body.sent_at,
        kind: body.kind.as_str().to_string(),
        text: body.text.clone(),
        reply_to: body.reply_to.clone(),
        media_json: incoming_media_json(&body)?,
        status: MessageStatus::Delivered,
        ratchet_header: None,
        ciphertext: None,
        signature: None,
        delivered_to: 0,
        total_members: 0,
    })?;
    for (index, part) in body.parts().iter().enumerate() {
        register_transfer(
            inner,
            account_id,
            &sender,
            &body.message_id,
            index as u32,
            &part.meta,
            &part.key,
        )?;
    }
    if let Some(record) = inner.store.message(&body.message_id)? {
        inner
            .emit(Event::MessengerGroupMessageReceived {
                account_id: account_id.to_string(),
                message: serde_json::to_value(MessageView::from_record(&record))
                    .unwrap_or_default(),
            })
            .await;
    }
    inner.emit_conversations(account_id).await;
    Ok(())
}

/// Accept one sealed chunk. Progress is reported per chunk so the progress bar
/// reflects what actually arrived; the assembled file is published only after
/// the whole file's digest matches.
async fn accept_chunk(
    inner: &Arc<Inner>,
    account_id: &str,
    transfer_id: &str,
    index: u32,
    sealed: &[u8],
    from_user_id: Option<&str>,
) -> Result<(u32, u32)> {
    let (meta, complete, received) = {
        let mut transfers = inner.transfers.lock().unwrap();
        let transfer = transfers
            .get_mut(transfer_id)
            .context("no transfer with that id is waiting for chunks")?;
        if transfer.account_id != account_id {
            bail!("that transfer belongs to another account");
        }
        // A transfer id is random, but knowing one must not let a third party
        // inject chunks into somebody else's file: the sender has to match the
        // peer the transfer was opened for.
        if let Some(from) = from_user_id
            && !transfer.peer_user_id.is_empty()
            && transfer.peer_user_id != from
        {
            bail!("that transfer belongs to another peer");
        }
        transfer.incoming.accept(index, sealed)?;
        let received = transfer.incoming.progress();
        let complete = transfer.incoming.complete();
        (transfer.meta.clone(), complete, received)
    };
    if complete {
        let finished = inner.transfers.lock().unwrap().remove(transfer_id);
        if let Some(transfer) = finished {
            let file_name = safe_file_name(&transfer.meta.file_name);
            let directory = inner.store.media_dir().join(&transfer.message_id);
            let destination = directory.join(file_name);
            let result = tokio::task::spawn_blocking(move || {
                transfer.incoming.finish(&destination).map(|_| destination)
            })
            .await
            .context("the attachment worker failed")?;
            match result {
                Ok(path) => {
                    inner
                        .store
                        .set_transfer_progress(transfer_id, received, "complete")?;
                    if let Some(mut record) = inner.store.message(transfer_id)? {
                        // The local path is what lets the UI open the file.
                        let mut media: serde_json::Value = record
                            .media_json
                            .as_deref()
                            .and_then(|encoded| serde_json::from_str(encoded).ok())
                            .unwrap_or_default();
                        if let Some(object) = media.as_object_mut() {
                            object.insert(
                                "local_path".into(),
                                serde_json::Value::String(path.display().to_string()),
                            );
                            record.media_json = Some(serde_json::to_string(&media)?);
                        }
                        inner
                            .emit(Event::MessengerMessageReceived {
                                account_id: account_id.to_string(),
                                message: serde_json::to_value(MessageView::from_record(&record))
                                    .unwrap_or_default(),
                            })
                            .await;
                    }
                    inner
                        .emit_media(
                            account_id,
                            transfer_id,
                            transfer_id,
                            "incoming",
                            &meta.file_name,
                            received,
                            meta.total_chunks,
                            "complete",
                        )
                        .await;
                }
                Err(error) => {
                    inner
                        .store
                        .set_transfer_progress(transfer_id, received, "failed")?;
                    inner
                        .emit_media(
                            account_id,
                            transfer_id,
                            transfer_id,
                            "incoming",
                            &meta.file_name,
                            received,
                            meta.total_chunks,
                            "failed",
                        )
                        .await;
                    bail!("the attachment could not be published: {error}");
                }
            }
        }
    } else {
        inner
            .store
            .set_transfer_progress(transfer_id, received, "active")?;
        inner
            .emit_media(
                account_id,
                transfer_id,
                transfer_id,
                "incoming",
                &meta.file_name,
                received,
                meta.total_chunks,
                "active",
            )
            .await;
    }
    Ok((received, meta.total_chunks))
}

/// A peer-supplied file name must never escape the media directory.
fn safe_file_name(name: &str) -> String {
    let trimmed = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = trimmed
        .chars()
        .filter(|character| !character.is_control() && *character != ':' && *character != '\0')
        .collect();
    let cleaned = cleaned.trim().trim_matches('.').to_string();
    if cleaned.is_empty() || cleaned == ".." {
        "attachment".into()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messenger::peer::MeshHandle;

    /// A Tor stand-in: it publishes one stable onion per channel and exposes a
    /// SOCKS port nobody listens on. Everything above the transport — framing,
    /// X3DH, ratchet, queueing — is the production code path.
    struct FakeMesh {
        socks: u16,
    }

    impl MeshHandle for FakeMesh {
        fn request(&self, action: &str, payload: &serde_json::Value) -> Result<serde_json::Value> {
            let channel = payload["channel"].as_str().unwrap_or_default();
            let onion = format!("{}{}", "a".repeat(52), "2345");
            Ok(match action {
                "mesh_start" | "mesh_status" => serde_json::json!({
                    "status": "Tor ready",
                    "socks_port": self.socks,
                    "onions": if channel.is_empty() {
                        serde_json::json!({})
                    } else {
                        serde_json::json!({ channel: onion })
                    },
                }),
                "mesh_revoke" => {
                    serde_json::json!({"status":"Tor ready","socks_port":self.socks,"onions":{}})
                }
                _ => serde_json::json!({}),
            })
        }
    }

    /// A directory under the system temp root that removes itself, so the
    /// messenger tests need no extra dependency.
    struct TempDir(std::path::PathBuf);

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    async fn service() -> (MessengerService, mpsc::Receiver<Event>, TempDir) {
        let directory = TempDir(
            std::env::temp_dir().join(format!("gnomeai-messenger-{}", crypto::random_id())),
        );
        let store = MessengerStore::open(
            &directory.path().join("store/messenger/messenger.db"),
            directory.path(),
        )
        .unwrap();
        let (events, rx) = mpsc::channel(256);
        let mesh = Arc::new(FakeMesh { socks: 1 });
        (MessengerService::new(store, mesh, events), rx, directory)
    }

    impl TempDir {
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    #[tokio::test]
    async fn accounts_are_separate_identities_with_separate_onions() {
        let (service, mut events, _directory) = service().await;
        service
            .handle_op(Op::MessengerCreateAccount {
                display_name: "Work".into(),
                pro: false,
            })
            .await
            .unwrap();
        service
            .handle_op(Op::MessengerCreateAccount {
                display_name: "Personal".into(),
                pro: false,
            })
            .await
            .unwrap();
        let accounts = service.store().accounts().unwrap();
        assert_eq!(accounts.len(), 2);
        assert_ne!(accounts[0].user_id, accounts[1].user_id);
        assert_ne!(accounts[0].identity_pk, accounts[1].identity_pk);
        assert_ne!(
            peer::channel_for(&accounts[0].account_id),
            peer::channel_for(&accounts[1].account_id)
        );
        // Walk past the event the first creation emitted.
        let accounts_event = loop {
            if let Some(Event::MessengerAccountsChanged { accounts }) = events.recv().await
                && accounts.len() == 2
            {
                break accounts;
            }
        };
        assert_eq!(accounts_event.len(), 2);
    }

    #[tokio::test]
    async fn switching_accounts_does_not_touch_the_device_mesh() {
        let (service, _events, _directory) = service().await;
        service
            .handle_op(Op::MessengerCreateAccount {
                display_name: "A".into(),
                pro: true,
            })
            .await
            .unwrap();
        service
            .handle_op(Op::MessengerCreateAccount {
                display_name: "B".into(),
                pro: true,
            })
            .await
            .unwrap();
        let second = service.store().accounts().unwrap()[1].account_id.clone();
        service
            .handle_op(Op::MessengerSwitchAccount {
                account_id: second.clone(),
            })
            .await
            .unwrap();
        let active = service.store().active_account().unwrap().unwrap();
        assert_eq!(active.account_id, second);
        assert!(service.store().accounts().unwrap()[0].account_id != second);
    }

    #[test]
    fn an_invite_must_be_signed_by_the_identity_it_names() {
        let (_, _, _directory) = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(async { service().await });
        let identity = crypto::IdentityKeys::generate();
        let prekey = x25519_dalek::StaticSecret::from(crypto::random_key());
        let public = x25519_dalek::PublicKey::from(&prekey);
        let signature = crypto::sign(
            &identity.signing(),
            &[
                b"gnomeai-messenger-signed-prekey-v1".as_slice(),
                public.as_bytes(),
            ]
            .concat(),
        );
        let good = serde_json::json!({
            "v": 1,
            "user_id": hex::encode(identity.signing_public().to_bytes()),
            "identity_pk": hex::encode(identity.dh_public()),
            "identity_public": hex::encode(identity.signing_public().to_bytes()),
            "signed_prekey": hex::encode(public.as_bytes()),
            "spk_sig": signature,
            "onion": format!("{}{}", "a".repeat(52), "2345"),
            "name": "Alice",
        });
        let contact = parse_invite(&good.to_string()).unwrap();
        assert_eq!(contact.display_name, "Alice");
        assert!(contact.verified);

        // A name swap: the user id claims one identity but signs with another.
        let other = crypto::IdentityKeys::generate();
        let mut forged = good.clone();
        forged["user_id"] = serde_json::json!(hex::encode(other.signing_public().to_bytes()));
        assert!(parse_invite(&forged.to_string()).is_err());

        // A prekey that the identity never signed.
        let mut unsigned = good.clone();
        unsigned["signed_prekey"] = serde_json::json!(hex::encode(
            x25519_dalek::PublicKey::from(&x25519_dalek::StaticSecret::from(crypto::random_key()))
                .as_bytes()
        ));
        assert!(parse_invite(&unsigned.to_string()).is_err());
    }

    #[tokio::test]
    async fn a_direct_message_is_queued_and_the_row_is_pending() {
        let (service, _events, _directory) = service().await;
        service
            .handle_op(Op::MessengerCreateAccount {
                display_name: "Me".into(),
                pro: false,
            })
            .await
            .unwrap();
        let account = service.store().accounts().unwrap().remove(0);
        let contact = contact_fixture("friend");
        service
            .store()
            .upsert_contact(&account.account_id, &contact)
            .unwrap();
        service
            .handle_op(Op::MessengerSendText {
                account_id: account.account_id.clone(),
                to: contact.user_id.clone(),
                text: "**hello**".into(),
                reply_to: None,
                group: false,
            })
            .await
            .unwrap();
        let conversation = service
            .store()
            .direct_conversation(&account.account_id, &contact.user_id)
            .unwrap();
        let messages = service
            .store()
            .messages(&account.account_id, &conversation)
            .unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].status, MessageStatus::Pending);
        assert_eq!(messages[0].text.as_deref(), Some("**hello**"));
        assert_eq!(service.store().outbox_len(&account.account_id).unwrap(), 1);
    }

    #[tokio::test]
    async fn editing_keeps_the_previous_version_and_resends_an_edit() {
        let (service, _events, _directory) = service().await;
        service
            .handle_op(Op::MessengerCreateAccount {
                display_name: "Me".into(),
                pro: false,
            })
            .await
            .unwrap();
        let account = service.store().accounts().unwrap().remove(0);
        let contact = contact_fixture("friend");
        service
            .store()
            .upsert_contact(&account.account_id, &contact)
            .unwrap();
        service
            .handle_op(Op::MessengerSendText {
                account_id: account.account_id.clone(),
                to: contact.user_id.clone(),
                text: "first".into(),
                reply_to: None,
                group: false,
            })
            .await
            .unwrap();
        let conversation = service
            .store()
            .direct_conversation(&account.account_id, &contact.user_id)
            .unwrap();
        let id = service
            .store()
            .messages(&account.account_id, &conversation)
            .unwrap()[0]
            .id
            .clone();
        service
            .handle_op(Op::MessengerEditMessage {
                account_id: account.account_id.clone(),
                id: id.clone(),
                new_text: "second".into(),
            })
            .await
            .unwrap();
        let record = service.store().message(&id).unwrap().unwrap();
        assert_eq!(record.text.as_deref(), Some("second"));
        assert!(record.edited);
        // The original message plus the edit control message are both queued.
        assert!(service.store().outbox_len(&account.account_id).unwrap() >= 2);
    }

    #[tokio::test]
    async fn a_group_is_capped_and_its_members_are_fanned_out() {
        let (service, _events, _directory) = service().await;
        service
            .handle_op(Op::MessengerCreateAccount {
                display_name: "Me".into(),
                pro: false,
            })
            .await
            .unwrap();
        let account = service.store().accounts().unwrap().remove(0);
        let mut ids = Vec::new();
        for index in 0..3 {
            let contact = contact_fixture(&format!("friend-{index}"));
            service
                .store()
                .upsert_contact(&account.account_id, &contact)
                .unwrap();
            ids.push(contact.user_id);
        }
        service
            .handle_op(Op::MessengerCreateGroup {
                account_id: account.account_id.clone(),
                name: "Team".into(),
                member_ids: ids.clone(),
            })
            .await
            .unwrap();
        let groups = service.store().groups(&account.account_id).unwrap();
        assert_eq!(groups.len(), 1);
        // Creator plus invitees.
        assert_eq!(groups[0].members.len(), 4);
        assert!(groups[0].is_admin(&account.user_id));
        service
            .handle_op(Op::MessengerSendText {
                account_id: account.account_id.clone(),
                to: groups[0].group_id.clone(),
                text: "hi team".into(),
                reply_to: None,
                group: true,
            })
            .await
            .unwrap();
        // One queued copy per peer: the fan-out is O(n) messages, not one blob.
        // Creating the group also queued one sender-key distribution per peer,
        // so count only the group messages here.
        let group_items = service
            .store()
            .outbox(&account.account_id)
            .unwrap()
            .into_iter()
            .filter(|item| {
                serde_json::from_str::<QueuedItem>(&item.request_json)
                    .map(|item| item.kind == "group")
                    .unwrap_or(false)
            })
            .count();
        assert_eq!(group_items, 3);
    }

    #[tokio::test]
    async fn removing_a_member_rotates_every_sender_chain() {
        let (service, _events, _directory) = service().await;
        service
            .handle_op(Op::MessengerCreateAccount {
                display_name: "Me".into(),
                pro: false,
            })
            .await
            .unwrap();
        let account = service.store().accounts().unwrap().remove(0);
        let contact = contact_fixture("friend");
        service
            .store()
            .upsert_contact(&account.account_id, &contact)
            .unwrap();
        service
            .handle_op(Op::MessengerCreateGroup {
                account_id: account.account_id.clone(),
                name: "Pair".into(),
                member_ids: vec![contact.user_id.clone()],
            })
            .await
            .unwrap();
        let group = service
            .store()
            .groups(&account.account_id)
            .unwrap()
            .remove(0);
        service
            .store()
            .save_sender_chain(
                &account.account_id,
                &group.group_id,
                &contact.user_id,
                &SenderChain::new(crypto::random_key()),
            )
            .unwrap();
        service
            .handle_op(Op::MessengerRemoveGroupMember {
                account_id: account.account_id.clone(),
                group_id: group.group_id.clone(),
                user_id: contact.user_id.clone(),
            })
            .await
            .unwrap();
        assert!(
            service
                .store()
                .sender_chain(&account.account_id, &group.group_id, &contact.user_id)
                .unwrap()
                .is_none(),
            "the removed member's chain must not survive the rotation"
        );
    }

    fn contact_fixture(name: &str) -> MessengerContact {
        let identity = crypto::IdentityKeys::generate();
        let prekey = x25519_dalek::StaticSecret::from(crypto::random_key());
        MessengerContact {
            user_id: hex::encode(identity.signing_public().to_bytes()),
            display_name: name.to_string(),
            identity_pk: hex::encode(identity.dh_public()),
            signed_prekey: hex::encode(x25519_dalek::PublicKey::from(&prekey).as_bytes()),
            onion_address: format!("{}{}", "b".repeat(52), "2345"),
            verified: true,
        }
    }

    /// A contact request must be visible in the transcript, not only in the
    /// banner above the conversation list.
    ///
    /// The banner is only drawn on the Chat home screen of the receiving
    /// account, so somebody sitting inside another conversation — the normal
    /// case on a phone — saw an invitation arrive and nothing appear. The note
    /// is also what makes the arrival survive a restart.
    #[tokio::test]
    async fn an_incoming_request_leaves_a_visible_note_and_no_duplicate() {
        let (owner, _events, _directory) = service().await;
        owner
            .handle_op(Op::MessengerCreateAccount {
                display_name: "Owner".into(),
                pro: true,
            })
            .await
            .unwrap();
        let account = owner.store().accounts().unwrap().remove(0);

        // A stranger with a genuinely signed invite, exactly as a scanner sends.
        let (stranger, invite) = signed_stranger("Stranger");
        let stranger_id = stranger.user_id;

        let first = handle_request(
            &owner.inner,
            &account.account_id,
            Request::ContactRequest {
                invite: invite.clone(),
            },
        )
        .await;
        assert!(matches!(first, Reply::ContactPending { .. }));

        let conversation = owner
            .store()
            .direct_conversation(&account.account_id, &stranger_id)
            .unwrap();
        let notes = owner
            .store()
            .messages(&account.account_id, &conversation)
            .unwrap();
        assert_eq!(notes.len(), 1, "the request must appear in the transcript");
        assert_eq!(notes[0].kind, "system");
        assert_eq!(notes[0].direction, "incoming");
        let text = notes[0].text.clone().unwrap_or_default();
        assert!(text.contains("Stranger"));
        assert!(text.contains("wants to chat"));

        // The sender re-asks on every heartbeat while nobody answers. That must
        // not stack a second note, nor a second notification.
        let again = handle_request(
            &owner.inner,
            &account.account_id,
            Request::ContactRequest { invite },
        )
        .await;
        assert!(matches!(again, Reply::ContactPending { .. }));
        assert_eq!(
            owner
                .store()
                .messages(&account.account_id, &conversation)
                .unwrap()
                .len(),
            1,
            "a retried request must not duplicate its note"
        );
    }

    /// Answering removes the note: it claimed somebody was still waiting.
    #[tokio::test]
    async fn answering_a_request_removes_its_transcript_note() {
        let (owner, _events, _directory) = service().await;
        owner
            .handle_op(Op::MessengerCreateAccount {
                display_name: "Owner".into(),
                pro: true,
            })
            .await
            .unwrap();
        let account = owner.store().accounts().unwrap().remove(0);
        // Answering re-reads the stored invite and re-verifies its signature, so
        // this fixture has to be a genuinely signed one — a placeholder would be
        // rejected before the note was ever touched.
        let (stranger, invite) = signed_stranger("Stranger");
        let conversation = owner
            .store()
            .direct_conversation(&account.account_id, &stranger.user_id)
            .unwrap();
        owner
            .store()
            .upsert_contact_request(&account.account_id, &stranger, &invite, "incoming")
            .unwrap();
        assert!(
            note_contact_request(&owner.inner, &account.account_id, &stranger)
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(
            owner
                .store()
                .messages(&account.account_id, &conversation)
                .unwrap()
                .len(),
            1
        );

        // Declining is the cheaper path: it sends no answer over Tor in this
        // test double, and still has to clear both the row and the note.
        owner
            .handle_op(Op::MessengerRespondContactRequest {
                account_id: account.account_id.clone(),
                user_id: stranger.user_id.clone(),
                accept: false,
            })
            .await
            .unwrap();
        assert!(
            owner
                .store()
                .contact_requests(&account.account_id)
                .unwrap()
                .is_empty()
        );
        assert!(
            owner
                .store()
                .messages(&account.account_id, &conversation)
                .unwrap()
                .is_empty(),
            "the note must not outlive the request it announced"
        );
    }

    /// An unanswered request must be re-sent, or the other device never learns
    /// it was asked.
    ///
    /// The first attempt is one Tor round trip. If the peer's Tor was still
    /// bootstrapping, or the phone was asleep, that attempt is lost — and with
    /// it the only chance the other side had to show the approval. Every
    /// message queued behind the request then stays `Pending` for good.
    #[tokio::test]
    async fn an_unanswered_request_is_retried_instead_of_being_sent_once() {
        let (scanner, _events, _directory) = service().await;
        scanner
            .handle_op(Op::MessengerCreateAccount {
                display_name: "Scanner".into(),
                pro: true,
            })
            .await
            .unwrap();
        let account = scanner.store().accounts().unwrap().remove(0);
        let (contact, invite) = signed_stranger("Owner");

        // Scanning writes the outgoing request before the network attempt, so
        // the row exists even though this test double cannot reach an onion.
        scanner
            .handle_op(Op::MessengerAddContact {
                account_id: account.account_id.clone(),
                invite,
            })
            .await
            .unwrap();
        let pending = scanner
            .store()
            .contact_requests(&account.account_id)
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].direction, "outgoing");

        // A retry must go out again, not treat the row as already answered: the
        // onion is unreachable here, so the outcome is `Unreachable` and the row
        // has to survive for the next tick.
        retry_pending_requests(&scanner.inner, &account.account_id, &account).await;
        let after = scanner
            .store()
            .contact_requests(&account.account_id)
            .unwrap();
        assert_eq!(
            after.len(),
            1,
            "an unreachable peer keeps the request queued"
        );
        assert_eq!(after[0].user_id, contact.user_id);
    }

    /// A stranger whose invite genuinely verifies: the same shape a QR carries.
    fn signed_stranger(name: &str) -> (MessengerContact, String) {
        let identity = crypto::IdentityKeys::generate();
        let prekey = x25519_dalek::StaticSecret::from(crypto::random_key());
        let prekey_public = x25519_dalek::PublicKey::from(&prekey);
        let signature = crypto::sign(
            &identity.signing(),
            &[
                b"gnomeai-messenger-signed-prekey-v1".as_slice(),
                prekey_public.as_bytes(),
            ]
            .concat(),
        );
        let user_id = hex::encode(identity.signing_public().to_bytes());
        let invite = serde_json::json!({
            "v": 1,
            "user_id": user_id,
            "identity_pk": hex::encode(identity.dh_public()),
            "identity_public": user_id,
            "signed_prekey": hex::encode(prekey_public.as_bytes()),
            "spk_sig": signature,
            "onion": format!("{}{}", "d".repeat(52), "2345"),
            "name": name,
        })
        .to_string();
        let contact = MessengerContact {
            user_id,
            display_name: name.to_string(),
            identity_pk: hex::encode(identity.dh_public()),
            signed_prekey: hex::encode(prekey_public.as_bytes()),
            onion_address: format!("{}{}", "d".repeat(52), "2345"),
            verified: true,
        };
        (contact, invite)
    }

    /// Scanning a chat QR is a *request*: the scanner keeps the contact so its
    /// composer works, the other device stores a pending request, and only an
    /// explicit answer turns it into a mutual contact.
    #[tokio::test]
    async fn a_scanned_invite_asks_the_other_device_and_needs_an_answer() {
        let scanner = service().await;
        let owner = service().await;
        for (service, name) in [(&scanner.0, "Scanner"), (&owner.0, "Owner")] {
            service
                .handle_op(Op::MessengerCreateAccount {
                    display_name: name.into(),
                    pro: true,
                })
                .await
                .unwrap();
        }
        let scanner_account = scanner.0.store().accounts().unwrap().remove(0);
        let owner_account = owner.0.store().accounts().unwrap().remove(0);
        let invite = invite_payload(&owner_account).to_string();

        // The request cannot reach a loopback onion in this test double, so the
        // pending row the sender writes is what we assert on: it is what keeps
        // the request alive until the owner's device answers.
        scanner
            .0
            .handle_op(Op::MessengerAddContact {
                account_id: scanner_account.account_id.clone(),
                invite: invite.clone(),
            })
            .await
            .unwrap();
        let outgoing = scanner
            .0
            .store()
            .contact_requests(&scanner_account.account_id)
            .unwrap();
        assert_eq!(outgoing.len(), 1);
        assert_eq!(outgoing[0].direction, "outgoing");
        assert_eq!(outgoing[0].user_id, owner_account.user_id);
        // The scanner may write locally, so it can type and queue right away.
        assert!(
            scanner
                .0
                .store()
                .contact(&scanner_account.account_id, &owner_account.user_id)
                .unwrap()
                .is_some()
        );
        // The owner has not agreed to anything yet.
        assert!(
            owner
                .0
                .store()
                .contact(&owner_account.account_id, &scanner_account.user_id)
                .unwrap()
                .is_none()
        );
    }

    /// A request that arrived over the onion lives in the pending table until
    /// the owner answers; accepting writes the contact and clears the request.
    #[tokio::test]
    async fn an_incoming_request_creates_the_contact_only_after_acceptance() {
        let owner = service().await;
        owner
            .0
            .handle_op(Op::MessengerCreateAccount {
                display_name: "Owner".into(),
                pro: true,
            })
            .await
            .unwrap();
        let account = owner.0.store().accounts().unwrap().remove(0);
        let stranger = contact_fixture("Stranger");
        let stranger_invite = serde_json::json!({
            "v": 1,
            "user_id": stranger.user_id,
            "identity_pk": stranger.identity_pk,
            "identity_public": stranger.user_id,
            "signed_prekey": stranger.signed_prekey,
            "spk_sig": "00",
            "onion": stranger.onion_address,
            "name": stranger.display_name,
        });

        // A request whose invite signature does not verify must not reach the
        // pending table at all: an unsigned knock changes no state.
        let reply = handle_request(
            &owner.0.inner,
            &account.account_id,
            Request::ContactRequest {
                invite: stranger_invite.to_string(),
            },
        )
        .await;
        assert!(matches!(reply, Reply::Error { .. }));
        assert!(
            owner
                .0
                .store()
                .contact_requests(&account.account_id)
                .unwrap()
                .is_empty()
        );
        assert!(
            owner
                .0
                .store()
                .contact(&account.account_id, &stranger.user_id)
                .unwrap()
                .is_none()
        );
    }

    /// A one-time prekey is spent when a preamble uses it, not when it is offered.
    ///
    /// Offering it and deleting it in the same step broke every honest handshake:
    /// the peer receives the key, seals its first message with it, and this side
    /// finds no secret to answer with — "the one-time prekey was already used" on
    /// a key that had never actually been used.
    #[tokio::test]
    async fn an_offered_prekey_survives_until_a_preamble_consumes_it() {
        let (service, _events, _directory) = service().await;
        service
            .handle_op(Op::MessengerCreateAccount {
                display_name: "Owner".into(),
                pro: false,
            })
            .await
            .unwrap();
        let account = service.store().accounts().unwrap().remove(0);
        // Account creation already tops the pool up; the count is what the
        // offering below must leave untouched.
        let initial = service
            .store()
            .one_time_prekey_count(&account.account_id)
            .unwrap();
        assert!(
            initial > 0,
            "a new account must start with prekeys to offer"
        );

        // Offering the same key twice must return the same key and keep its
        // secret available: a bundle is an offer, not a spend.
        let offered = service
            .store()
            .take_one_time_prekey(&account.account_id)
            .unwrap()
            .expect("a topped-up pool must have a key to offer");
        let again = service
            .store()
            .take_one_time_prekey(&account.account_id)
            .unwrap()
            .expect("an offered key stays offerable");
        assert_eq!(offered.public, again.public);
        assert_eq!(
            service
                .store()
                .one_time_prekey_count(&account.account_id)
                .unwrap(),
            initial,
            "offering must not drain the pool"
        );

        // The responder consumes it exactly once, and keeps the secret.
        let consumed = service
            .store()
            .consume_one_time_prekey(&account.account_id, &offered.public)
            .unwrap()
            .expect("the offered key must still be in the pool when the preamble arrives");
        assert_eq!(consumed.secret, offered.secret);
        assert!(
            service
                .store()
                .consume_one_time_prekey(&account.account_id, &offered.public)
                .unwrap()
                .is_none(),
            "the same preamble must not be accepted twice"
        );
    }

    /// A verified request really becomes a visible pending row, and answering
    /// it is what writes the contact — the exact flow the UI drives.
    #[tokio::test]
    async fn a_verified_request_is_pending_until_it_is_answered() {
        let owner = service().await;
        owner
            .0
            .handle_op(Op::MessengerCreateAccount {
                display_name: "Owner".into(),
                pro: true,
            })
            .await
            .unwrap();
        let account = owner.0.store().accounts().unwrap().remove(0);

        // A stranger with a genuinely signed invite, as a scanner would send.
        let identity = crypto::IdentityKeys::generate();
        let prekey = x25519_dalek::StaticSecret::from(crypto::random_key());
        let prekey_public = x25519_dalek::PublicKey::from(&prekey);
        let signature = crypto::sign(
            &identity.signing(),
            &[
                b"gnomeai-messenger-signed-prekey-v1".as_slice(),
                prekey_public.as_bytes(),
            ]
            .concat(),
        );
        let stranger_user_id = hex::encode(identity.signing_public().to_bytes());
        let invite = serde_json::json!({
            "v": 1,
            "user_id": stranger_user_id,
            "identity_pk": hex::encode(identity.dh_public()),
            "identity_public": stranger_user_id,
            "signed_prekey": hex::encode(prekey_public.as_bytes()),
            "spk_sig": signature,
            "onion": format!("{}{}", "c".repeat(52), "2345"),
            "name": "Stranger",
        })
        .to_string();

        let reply = handle_request(
            &owner.0.inner,
            &account.account_id,
            Request::ContactRequest { invite },
        )
        .await;
        // Delivered, and deliberately not yet a contact.
        assert!(matches!(reply, Reply::ContactPending { .. }));
        let pending = owner
            .0
            .store()
            .contact_requests(&account.account_id)
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].direction, "incoming");
        assert_eq!(pending[0].display_name, "Stranger");
        assert!(
            owner
                .0
                .store()
                .contact(&account.account_id, &stranger_user_id)
                .unwrap()
                .is_none()
        );

        // The user answers; the contact appears and the request disappears. The
        // peer is unreachable in this test, which must not block the local write.
        owner
            .0
            .handle_op(Op::MessengerRespondContactRequest {
                account_id: account.account_id.clone(),
                user_id: stranger_user_id.clone(),
                accept: true,
            })
            .await
            .unwrap();
        assert!(
            owner
                .0
                .store()
                .contact(&account.account_id, &stranger_user_id)
                .unwrap()
                .is_some()
        );
        assert!(
            owner
                .0
                .store()
                .contact_requests(&account.account_id)
                .unwrap()
                .is_empty()
        );
    }

    /// A group message really is sealed once by the sender and opened by every
    /// other member with the sender key they were handed over 1:1.
    #[tokio::test]
    async fn a_group_message_reaches_every_member_from_one_ciphertext() {
        let alice = service().await;
        let bob = service().await;
        let carol = service().await;
        for (service, name) in [(&alice.0, "Alice"), (&bob.0, "Bob"), (&carol.0, "Carol")] {
            service
                .handle_op(Op::MessengerCreateAccount {
                    display_name: name.into(),
                    pro: true,
                })
                .await
                .unwrap();
        }
        let alice_account = alice.0.store().accounts().unwrap().remove(0);
        let bob_account = bob.0.store().accounts().unwrap().remove(0);
        let carol_account = carol.0.store().accounts().unwrap().remove(0);
        for service in [&alice.0, &bob.0, &carol.0] {
            let account = service.store().active_account().unwrap().unwrap();
            service
                .store()
                .ensure_one_time_prekeys(&account.account_id, 4)
                .unwrap();
        }

        // Alice knows both peers from their invites.
        for peer in [&bob_account, &carol_account] {
            alice
                .0
                .handle_op(Op::MessengerAddContact {
                    account_id: alice_account.account_id.clone(),
                    invite: invite_payload(peer).to_string(),
                })
                .await
                .unwrap();
        }
        alice
            .0
            .handle_op(Op::MessengerCreateGroup {
                account_id: alice_account.account_id.clone(),
                name: "Team".into(),
                member_ids: vec![bob_account.user_id.clone(), carol_account.user_id.clone()],
            })
            .await
            .unwrap();
        let group = alice
            .0
            .store()
            .groups(&alice_account.account_id)
            .unwrap()
            .remove(0);

        // Alice's sender key, as the distribution message would carry it.
        let chain = alice
            .0
            .store()
            .sender_chain(
                &alice_account.account_id,
                &group.group_id,
                &alice_account.user_id,
            )
            .unwrap()
            .unwrap();
        let distribution = SenderKeyDistribution {
            group_id: group.group_id.clone(),
            group_name: group.name.clone(),
            sender_id: alice_account.user_id.clone(),
            chain_key: base64::engine::general_purpose::STANDARD.encode(chain.to_bytes()),
            iteration: chain.iteration(),
            members: group.members.clone(),
            admins: group.admins.clone(),
            revoked: false,
        };

        // Both recipients install it over their own 1:1 channel.
        for service in [&bob.0, &carol.0] {
            let account = service.store().active_account().unwrap().unwrap();
            service
                .store()
                .upsert_contact(&account.account_id, &{
                    MessengerContact {
                        user_id: alice_account.user_id.clone(),
                        display_name: "Alice".into(),
                        identity_pk: alice_account.identity_pk.clone(),
                        signed_prekey: alice_account.signed_prekey.clone(),
                        onion_address: alice_account.onion_address.clone(),
                        verified: true,
                    }
                })
                .unwrap();
            // The distribution now travels *inside* a ratcheted envelope, so it
            // cannot be injected without a live 1:1 session. This test is about
            // the group path, so the chain is installed directly and the
            // distribution's own sealing is covered by the 1:1 test.
            let chain = SenderChain::from_bytes(
                base64::engine::general_purpose::STANDARD
                    .decode(&distribution.chain_key)
                    .unwrap(),
            )
            .unwrap();
            let wire = distribution_body(&distribution).unwrap();
            // The round trip through the body proves both sides agree on the
            // shape of a distribution before asserting on the group message.
            assert_eq!(
                sealed_distribution(&wire).unwrap().group_id,
                distribution.group_id
            );
            service
                .store()
                .save_sender_chain(
                    &account.account_id,
                    &group.group_id,
                    &alice_account.user_id,
                    &chain,
                )
                .unwrap();
            let mut group = group.clone();
            group.account_id = account.account_id.clone();
            service.store().save_group(&group).unwrap();
        }

        // Alice seals one ciphertext for the whole group.
        let body = MessageBody {
            message_id: crypto::random_id(),
            kind: MessageKind::Text,
            text: Some("group hello".into()),
            attachments: Vec::new(),
            attachment: None,
            attachment_key: None,
            reply_to: None,
            edit_of: None,
            group_id: Some(group.group_id.clone()),
            sender_id: Some(alice_account.user_id.clone()),
            sender_name: Some("Alice".into()),
            iteration: None,
            sent_at: chrono::Utc::now().timestamp_millis(),
        };
        let envelope = alice
            .0
            .seal_group(
                &alice_account.account_id,
                &group.group_id,
                &alice_account,
                &body,
            )
            .unwrap();
        for service in [&bob.0, &carol.0] {
            let account = service.store().active_account().unwrap().unwrap();
            let reply = handle_request(
                &service.inner,
                &account.account_id,
                Request::Group {
                    envelope: Box::new(envelope.clone()),
                },
            )
            .await;
            assert!(
                matches!(reply, Reply::Accepted),
                "a member rejected the group message: {reply:?}"
            );
            let conversation = service
                .store()
                .group_conversation(&account.account_id, &group.group_id)
                .unwrap();
            let stored = service
                .store()
                .messages(&account.account_id, &conversation)
                .unwrap();
            assert_eq!(stored.len(), 1);
            assert_eq!(stored[0].text.as_deref(), Some("group hello"));
            assert_eq!(stored[0].from_user_id, alice_account.user_id);
        }
    }

    #[test]
    fn file_names_from_a_peer_cannot_escape_the_media_directory() {
        assert_eq!(safe_file_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_file_name("C:\\Windows\\system32"), "system32");
        assert_eq!(safe_file_name(".."), "attachment");
        assert_eq!(safe_file_name(""), "attachment");
        assert_eq!(safe_file_name("holiday.png"), "holiday.png");
    }

    /// The sender-key distribution really is a 1:1 body: it round-trips through
    /// the same shape and is refused when a `SenderKey` request tries to carry
    /// something that is not a distribution.
    #[test]
    fn a_sender_key_distribution_is_a_sealed_1to1_body() {
        let distribution = SenderKeyDistribution {
            group_id: "group-1".into(),
            group_name: "Team".into(),
            sender_id: "alice".into(),
            chain_key: base64::engine::general_purpose::STANDARD.encode(vec![7u8; 40]),
            iteration: 3,
            members: vec![GroupMemberWire {
                user_id: "alice".into(),
                onion: "a".repeat(56),
                display_name: "Alice".into(),
                identity_pk: "00".into(),
                signed_prekey: "00".into(),
            }],
            admins: vec!["alice".into()],
            revoked: false,
        };
        let body = distribution_body(&distribution).unwrap();
        let decoded = sealed_distribution(&body).unwrap();
        assert_eq!(decoded, distribution);

        // A plain chat message must not be accepted as a distribution.
        let chat = MessageBody {
            message_id: "m1".into(),
            kind: MessageKind::Text,
            text: Some("hello".into()),
            attachments: Vec::new(),
            attachment: None,
            attachment_key: None,
            reply_to: None,
            edit_of: None,
            group_id: None,
            sender_id: None,
            sender_name: None,
            iteration: None,
            sent_at: 0,
        };
        assert!(sealed_distribution(&chat).is_err());
    }

    /// A burst of plain messages to one peer must be sealable as a batch.
    ///
    /// The saving being protected here is not cosmetic: `exchange` opens a Tor
    /// circuit per message, and circuit setup is what a person experiences as
    /// lag. Batching only applies to the plain, attachment-free, same-peer case,
    /// so the classifier that decides it has to keep saying yes to that case and
    /// no to the rest.
    #[tokio::test]
    async fn plain_messages_to_one_peer_are_batchable() {
        let (service, _events, _directory) = service().await;
        service
            .handle_op(Op::MessengerCreateAccount {
                display_name: "Me".into(),
                pro: false,
            })
            .await
            .unwrap();
        let account = service.store().accounts().unwrap().remove(0);
        let contact = contact_fixture("friend");
        service
            .store()
            .upsert_contact(&account.account_id, &contact)
            .unwrap();
        let queued = |kind: &str, attachments: bool| {
            let item = QueuedItem {
                kind: kind.into(),
                target: contact.user_id.clone(),
                message_id: crypto::random_id(),
                body: Some(MessageBody {
                    message_id: crypto::random_id(),
                    kind: MessageKind::Text,
                    text: Some("hi".into()),
                    attachments: Vec::new(),
                    attachment: None,
                    attachment_key: None,
                    reply_to: None,
                    edit_of: None,
                    group_id: None,
                    sender_id: None,
                    sender_name: None,
                    iteration: None,
                    sent_at: 0,
                }),
                group_envelope: None,
                distribution: None,
                receipt: None,
                attachments: if attachments {
                    vec![QueuedAttachment {
                        path: "/tmp/x".into(),
                        key: "00".into(),
                        index: 0,
                    }]
                } else {
                    Vec::new()
                },
                attachment_path: None,
                attachment_key: None,
            };
            OutboxItem {
                id: crypto::random_id(),
                account_id: account.account_id.clone(),
                message_id: item.message_id.clone(),
                peer_user_id: contact.user_id.clone(),
                request_json: serde_json::to_string(&item).unwrap(),
                attempts: 0,
            }
        };

        // A plain message travels with the next plain message to the same peer.
        assert!(!remembers_sender_key(&queued("direct", false)));
        assert!(!has_attachments(&queued("direct", false)));
        // A sender key installs the chain the next message needs, so it is
        // never folded into a batch.
        assert!(remembers_sender_key(&queued("sender_key", false)));
        // Attachments stream their own chunks after the control message.
        assert!(has_attachments(&queued("direct", true)));
    }

    /// An offline peer must cost one attempt and then wait, not spin. The queue
    /// keeps the message (so nothing is lost) and the worker is released while a
    /// delayed retry is pending.
    #[tokio::test]
    async fn an_offline_peer_is_not_retried_in_a_hot_loop() {
        let (service, _events, _directory) = service().await;
        service
            .handle_op(Op::MessengerCreateAccount {
                display_name: "Me".into(),
                pro: false,
            })
            .await
            .unwrap();
        let account = service.store().accounts().unwrap().remove(0);
        let contact = contact_fixture("friend");
        service
            .store()
            .upsert_contact(&account.account_id, &contact)
            .unwrap();
        service
            .handle_op(Op::MessengerSendText {
                account_id: account.account_id.clone(),
                to: contact.user_id.clone(),
                text: "are you there?".into(),
                reply_to: None,
                group: false,
            })
            .await
            .unwrap();

        // Give the worker a moment to run against a SOCKS port nobody serves.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let attempts = service
            .store()
            .outbox(&account.account_id)
            .unwrap()
            .first()
            .map(|item| item.attempts)
            .unwrap_or(0);
        // Zero attempts mean the worker could not even reach Tor, one means it
        // tried and backed off. Both are fine; dozens would be a spin.
        assert!(
            attempts <= 1,
            "the queue retried {attempts} times without waiting"
        );
        // The message is still there, still pending.
        let conversation = service
            .store()
            .direct_conversation(&account.account_id, &contact.user_id)
            .unwrap();
        let stored = service
            .store()
            .messages(&account.account_id, &conversation)
            .unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].status, MessageStatus::Pending);
    }

    #[test]
    fn conversation_ids_round_trip() {
        let account = "abc123";
        assert_eq!(
            direct_peer_of(account, &format!("dm:{account}:peer")).as_deref(),
            Some("peer")
        );
        assert_eq!(
            group_of_conversation(account, &format!("grp:{account}:g1")).as_deref(),
            Some("g1")
        );
        assert!(direct_peer_of(account, "grp:abc123:g1").is_none());
    }

    #[test]
    fn previews_show_the_attachment_badge() {
        let row = ConversationSummary {
            conversation_id: "c".into(),
            kind: "direct".into(),
            target: "t".into(),
            unread: 0,
            updated_at: 0,
            last_kind: "image".into(),
            last_text: String::new(),
            last_media: String::new(),
        };
        assert_eq!(preview_of(&row), "🖼 photo");
        let with_caption = ConversationSummary {
            last_kind: "video".into(),
            last_text: "sunset".into(),
            ..row
        };
        assert_eq!(preview_of(&with_caption), "🎥 video · sunset");
    }

    /// Two real messenger accounts, wired to each other, exchanging one sealed
    /// message through the production send path and the production receive
    /// path. The transport is loopback because a live Tor circuit would make
    /// this test slow and unrepeatable; everything above TCP is real.
    #[tokio::test]
    async fn a_message_survives_the_full_send_and_receive_path() {
        let sender = service().await;
        let receiver = service().await;
        for (service, name) in [(&sender.0, "Sender"), (&receiver.0, "Receiver")] {
            service
                .handle_op(Op::MessengerCreateAccount {
                    display_name: name.into(),
                    pro: false,
                })
                .await
                .unwrap();
        }
        let sender_account = sender.0.store().accounts().unwrap().remove(0);
        let receiver_account = receiver.0.store().accounts().unwrap().remove(0);
        sender
            .0
            .store()
            .ensure_one_time_prekeys(&sender_account.account_id, 4)
            .unwrap();
        receiver
            .0
            .store()
            .ensure_one_time_prekeys(&receiver_account.account_id, 4)
            .unwrap();

        // The receiver's invite, exactly as a QR would carry it.
        let invite = invite_payload(&receiver_account).to_string();
        sender
            .0
            .handle_op(Op::MessengerAddContact {
                account_id: sender_account.account_id.clone(),
                invite,
            })
            .await
            .unwrap();

        // Seal one message with the sender's real identity and ratchet.
        let contact = sender
            .0
            .store()
            .contact(&sender_account.account_id, &receiver_account.user_id)
            .unwrap()
            .unwrap();
        let body = MessageBody {
            message_id: crypto::random_id(),
            kind: MessageKind::Text,
            text: Some("**hello** from the real path".into()),
            attachments: Vec::new(),
            attachment: None,
            attachment_key: None,
            reply_to: None,
            edit_of: None,
            group_id: None,
            sender_id: None,
            sender_name: None,
            iteration: None,
            sent_at: chrono::Utc::now().timestamp_millis(),
        };
        let envelope = seal_direct(
            &sender.0.inner,
            &sender_account.account_id,
            &sender_account,
            &contact,
            &serde_json::to_vec(&body).unwrap(),
        )
        .await
        .unwrap();
        assert!(
            envelope.preamble.is_some(),
            "the first message of a conversation must carry the X3DH preamble"
        );

        // The receiver is a *contact-less* stranger until the invite is imported,
        // so import it first: that is what proves the separation between
        // "someone knocked" and "someone I know knocked".
        let back = invite_payload(&sender_account).to_string();
        receiver
            .0
            .handle_op(Op::MessengerAddContact {
                account_id: receiver_account.account_id.clone(),
                invite: back,
            })
            .await
            .unwrap();
        let reply = handle_request(
            &receiver.0.inner,
            &receiver_account.account_id,
            Request::Message {
                envelope: Box::new(envelope),
            },
        )
        .await;
        assert!(
            matches!(reply, Reply::Accepted),
            "the receiver rejected an honest envelope: {reply:?}"
        );
        let conversation = receiver
            .0
            .store()
            .direct_conversation(&receiver_account.account_id, &sender_account.user_id)
            .unwrap();
        let stored = receiver
            .0
            .store()
            .messages(&receiver_account.account_id, &conversation)
            .unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(
            stored[0].text.as_deref(),
            Some("**hello** from the real path")
        );
        assert_eq!(stored[0].direction, "incoming");

        // A second message must reuse the session, not a fresh preamble.
        let again = seal_direct(
            &sender.0.inner,
            &sender_account.account_id,
            &sender_account,
            &contact,
            &serde_json::to_vec(&body).unwrap(),
        )
        .await
        .unwrap();
        assert!(again.preamble.is_none());
        let reply = handle_request(
            &receiver.0.inner,
            &receiver_account.account_id,
            Request::Message {
                envelope: Box::new(again),
            },
        )
        .await;
        assert!(matches!(reply, Reply::Accepted));
    }

    /// The regression that produced "the peer rejected the message: sealed data
    /// failed authentication" on two real devices.
    ///
    /// Once both sides hold each other's invite, both composers are enabled, so
    /// both can seal a first message before either has read one.
    /// Each seals under its own X3DH root; two roots are two keys. A single
    /// session per peer meant the side that read the peer's preamble then tried
    /// to open that peer's follow-ups with a root the peer never derived.
    #[tokio::test]
    async fn a_conversation_both_sides_start_is_still_readable() {
        let alice = service().await;
        let bob = service().await;
        for (service, name) in [(&alice.0, "Alice"), (&bob.0, "Bob")] {
            service
                .handle_op(Op::MessengerCreateAccount {
                    display_name: name.into(),
                    pro: true,
                })
                .await
                .unwrap();
        }
        let alice_account = alice.0.store().accounts().unwrap().remove(0);
        let bob_account = bob.0.store().accounts().unwrap().remove(0);
        for (service, account) in [(&alice.0, &alice_account), (&bob.0, &bob_account)] {
            service
                .store()
                .ensure_one_time_prekeys(&account.account_id, 4)
                .unwrap();
        }
        // Each side imported the other's invite, which is why both can initiate.
        alice
            .0
            .handle_op(Op::MessengerAddContact {
                account_id: alice_account.account_id.clone(),
                invite: invite_payload(&bob_account).to_string(),
            })
            .await
            .unwrap();
        bob.0
            .handle_op(Op::MessengerAddContact {
                account_id: bob_account.account_id.clone(),
                invite: invite_payload(&alice_account).to_string(),
            })
            .await
            .unwrap();
        // Both accepted, so neither side answers with `NotAContact`.
        alice
            .0
            .store()
            .upsert_contact(
                &alice_account.account_id,
                &parse_invite(&invite_payload(&bob_account).to_string()).unwrap(),
            )
            .unwrap();
        bob.0
            .store()
            .upsert_contact(
                &bob_account.account_id,
                &parse_invite(&invite_payload(&alice_account).to_string()).unwrap(),
            )
            .unwrap();

        let body = |text: &str| MessageBody {
            message_id: crypto::random_id(),
            kind: MessageKind::Text,
            text: Some(text.into()),
            attachments: Vec::new(),
            attachment: None,
            attachment_key: None,
            reply_to: None,
            edit_of: None,
            group_id: None,
            sender_id: None,
            sender_name: None,
            iteration: None,
            sent_at: chrono::Utc::now().timestamp_millis(),
        };
        let bob_contact = alice
            .0
            .store()
            .contact(&alice_account.account_id, &bob_account.user_id)
            .unwrap()
            .unwrap();
        let alice_contact = bob
            .0
            .store()
            .contact(&bob_account.account_id, &alice_account.user_id)
            .unwrap()
            .unwrap();

        // Both seal before either receives — the collision.
        let alice_first = seal_direct(
            &alice.0.inner,
            &alice_account.account_id,
            &alice_account,
            &bob_contact,
            &serde_json::to_vec(&body("hello from alice")).unwrap(),
        )
        .await
        .unwrap();
        let bob_first = seal_direct(
            &bob.0.inner,
            &bob_account.account_id,
            &bob_account,
            &alice_contact,
            &serde_json::to_vec(&body("hello from bob")).unwrap(),
        )
        .await
        .unwrap();
        assert!(alice_first.preamble.is_some());
        assert!(bob_first.preamble.is_some());

        // Each side reads the other's opening message, then answers on the
        // session it opened.
        for (service, account, envelope, label) in [
            (&bob.0, &bob_account, alice_first, "bob reads alice"),
            (&alice.0, &alice_account, bob_first, "alice reads bob"),
        ] {
            let reply = handle_request(
                &service.inner,
                &account.account_id,
                Request::FirstMessage {
                    envelope: Box::new(envelope),
                },
            )
            .await;
            assert!(
                matches!(reply, Reply::Accepted),
                "{label} failed: {reply:?}"
            );
        }

        // The follow-up each side now sends must arrive under the chain the peer
        // is reading, which is the whole point of separating the directions.
        let alice_second = seal_direct(
            &alice.0.inner,
            &alice_account.account_id,
            &alice_account,
            &bob_contact,
            &serde_json::to_vec(&body("second from alice")).unwrap(),
        )
        .await
        .unwrap();
        assert!(
            alice_second.preamble.is_none(),
            "the session must be reused, not re-initiated"
        );
        let reply = handle_request(
            &bob.0.inner,
            &bob_account.account_id,
            Request::Message {
                envelope: Box::new(alice_second),
            },
        )
        .await;
        assert!(
            matches!(reply, Reply::Accepted),
            "the follow-up could not be authenticated: {reply:?}"
        );
    }

    #[tokio::test]
    async fn a_stranger_cannot_write_into_the_store() {
        let (service, _events, _directory) = service().await;
        service
            .handle_op(Op::MessengerCreateAccount {
                display_name: "Me".into(),
                pro: false,
            })
            .await
            .unwrap();
        let account = service.store().accounts().unwrap().remove(0);
        // A well-formed handshake is fine: it hands out only a bundle.
        let reply = handle_request(
            &service.inner,
            &account.account_id,
            Request::Handshake {
                from_user_id: "x".repeat(64),
                from_identity_pk: "y".repeat(64),
                signature: "z".repeat(128),
                nonce: "n".repeat(32),
            },
        )
        .await;
        assert!(matches!(reply, Reply::Bundle { .. }));
        // A message from somebody we never added is refused — but with a
        // *retryable* answer, because the sender should keep its message queued
        // until this device accepts the request, not lose it.
        let stranger = contact_fixture("stranger");
        let reply = handle_request(
            &service.inner,
            &account.account_id,
            Request::Message {
                envelope: Box::new(Envelope {
                    v: PROTOCOL_VERSION,
                    from_user_id: stranger.user_id,
                    from_account_onion: String::new(),
                    header: crypto::RatchetHeader {
                        dh: hex::encode([1u8; 32]),
                        previous_chain_length: 0,
                        message_number: 0,
                    },
                    ciphertext: base64::engine::general_purpose::STANDARD.encode("nope"),
                    signature: "00".into(),
                    preamble: None,
                }),
            },
        )
        .await;
        assert!(matches!(reply, Reply::NotAContact { .. }));
        assert!(
            service
                .store()
                .conversations(&account.account_id)
                .unwrap()
                .is_empty()
        );
    }
}
