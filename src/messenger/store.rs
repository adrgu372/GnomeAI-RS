//! Messenger state, in its own SQLite file and its own keystore.
//!
//! Two deliberate separations from the rest of GnomeAI:
//!
//! * `store/messenger/messenger.db` holds only `messenger_*` tables. No
//!   device-mesh row is ever written here, and no messenger row is ever written
//!   into the agent session database.
//! * `store/messenger/keystore.key` is a distinct 32-byte master key, created
//!   `0600` and never derived from a device identity. It seals every messenger
//!   secret at rest: identity seeds, prekey secrets, ratchet state, attachment
//!   keys. Losing it loses the account; leaking a device key does not reveal it.

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use super::crypto::{self, IdentityKeys, Key};

/// Free tier: two identities per installation. Raising it is a local policy
/// decision, never a server call.
pub const FREE_ACCOUNT_LIMIT: usize = 2;
/// Groups are capped in the app, on both create and join.
pub const MAX_GROUP_MEMBERS: usize = 300;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS messenger_accounts (
    account_id        TEXT PRIMARY KEY,
    user_id           TEXT NOT NULL UNIQUE,
    display_name      TEXT NOT NULL,
    avatar            BLOB,
    onion_address     TEXT NOT NULL DEFAULT '',
    identity_pk       TEXT NOT NULL,
    identity_public   TEXT NOT NULL,
    identity_seed     BLOB NOT NULL,
    dh_seed           BLOB NOT NULL,
    signed_prekey     TEXT NOT NULL,
    signed_prekey_secret BLOB NOT NULL,
    prekey_signature  TEXT NOT NULL,
    one_time_prekeys  BLOB NOT NULL DEFAULT X'',
    created_at        INTEGER NOT NULL,
    active            INTEGER NOT NULL DEFAULT 0,
    pro               INTEGER NOT NULL DEFAULT 0,
    chunk_size        INTEGER NOT NULL DEFAULT 262144
);

CREATE TABLE IF NOT EXISTS messenger_contacts (
    account_id     TEXT NOT NULL,
    user_id        TEXT NOT NULL,
    display_name   TEXT NOT NULL,
    identity_pk    TEXT NOT NULL DEFAULT '',
    signed_prekey  TEXT NOT NULL DEFAULT '',
    onion_address  TEXT NOT NULL DEFAULT '',
    verified       INTEGER NOT NULL DEFAULT 0,
    created_at     INTEGER NOT NULL,
    PRIMARY KEY (account_id, user_id)
);

-- A scanned invite that is waiting for this device to accept it. Nothing here
-- may create a contact: a request is a proposal, and the contact row is written
-- only when the user says yes (or when this device sends its own request and
-- the other side accepts).
CREATE TABLE IF NOT EXISTS messenger_contact_requests (
    account_id     TEXT NOT NULL,
    user_id        TEXT NOT NULL,
    display_name   TEXT NOT NULL,
    identity_pk    TEXT NOT NULL DEFAULT '',
    signed_prekey  TEXT NOT NULL DEFAULT '',
    onion_address  TEXT NOT NULL DEFAULT '',
    invite_json    TEXT NOT NULL,
    direction      TEXT NOT NULL,
    created_at     INTEGER NOT NULL,
    PRIMARY KEY (account_id, user_id)
);

CREATE TABLE IF NOT EXISTS messenger_conversations (
    conversation_id TEXT PRIMARY KEY,
    account_id      TEXT NOT NULL,
    kind            TEXT NOT NULL,
    peer_user_id    TEXT NOT NULL DEFAULT '',
    group_id        TEXT NOT NULL DEFAULT '',
    unread          INTEGER NOT NULL DEFAULT 0,
    updated_at      INTEGER NOT NULL,
    UNIQUE (account_id, kind, peer_user_id)
);
CREATE INDEX IF NOT EXISTS idx_messenger_conversations_unread
    ON messenger_conversations(account_id, unread, updated_at);

CREATE TABLE IF NOT EXISTS messenger_messages (
    id              TEXT PRIMARY KEY,
    conversation_id TEXT NOT NULL,
    account_id      TEXT NOT NULL,
    direction       TEXT NOT NULL,
    from_user_id    TEXT NOT NULL,
    to_user_id      TEXT NOT NULL DEFAULT '',
    ts              INTEGER NOT NULL,
    kind            TEXT NOT NULL,
    text            TEXT,
    reply_to        TEXT,
    edited          INTEGER NOT NULL DEFAULT 0,
    edit_ts         INTEGER,
    media_json      TEXT,
    status          TEXT NOT NULL,
    ratchet_header  TEXT,
    ciphertext      TEXT,
    signature       TEXT,
    delivered_to    INTEGER NOT NULL DEFAULT 0,
    total_members   INTEGER NOT NULL DEFAULT 0,
    versions_json   TEXT NOT NULL DEFAULT '[]'
);
CREATE INDEX IF NOT EXISTS idx_messenger_messages_conversation
    ON messenger_messages(conversation_id, ts, id);
CREATE INDEX IF NOT EXISTS idx_messenger_messages_pending
    ON messenger_messages(account_id, status);

CREATE TABLE IF NOT EXISTS messenger_ratchets (
    account_id   TEXT NOT NULL,
    peer_user_id TEXT NOT NULL,
    state        BLOB NOT NULL,
    updated_at   INTEGER NOT NULL,
    PRIMARY KEY (account_id, peer_user_id)
);

CREATE TABLE IF NOT EXISTS messenger_groups (
    group_id    TEXT PRIMARY KEY,
    account_id  TEXT NOT NULL,
    name        TEXT NOT NULL,
    avatar      BLOB,
    created_by  TEXT NOT NULL,
    created_at  INTEGER NOT NULL,
    members     TEXT NOT NULL,
    admins      TEXT NOT NULL,
    revoked     INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_messenger_groups_account
    ON messenger_groups(account_id);

CREATE TABLE IF NOT EXISTS messenger_sender_keys (
    account_id TEXT NOT NULL,
    group_id   TEXT NOT NULL,
    sender_id  TEXT NOT NULL,
    chain      BLOB NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (account_id, group_id, sender_id)
);

CREATE TABLE IF NOT EXISTS messenger_outbox (
    id           TEXT PRIMARY KEY,
    account_id   TEXT NOT NULL,
    message_id   TEXT NOT NULL,
    peer_user_id TEXT NOT NULL,
    payload      BLOB NOT NULL,
    attempts     INTEGER NOT NULL DEFAULT 0,
    created_at   INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_messenger_outbox_account
    ON messenger_outbox(account_id, created_at);

CREATE TABLE IF NOT EXISTS messenger_transfers (
    transfer_id TEXT PRIMARY KEY,
    account_id  TEXT NOT NULL,
    peer_user_id TEXT NOT NULL,
    direction   TEXT NOT NULL,
    message_id  TEXT NOT NULL,
    path        TEXT NOT NULL DEFAULT '',
    meta_json   TEXT NOT NULL,
    key         BLOB NOT NULL,
    received    INTEGER NOT NULL DEFAULT 0,
    status      TEXT NOT NULL,
    updated_at  INTEGER NOT NULL
);
"#;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// The messenger keystore. It is a separate file on purpose: an alias-per-file
/// model keeps the messenger key material out of every device-mesh code path.
fn load_or_create_master_key(database: &Path) -> Result<Key> {
    let path = database
        .parent()
        .context("the messenger database has no parent directory")?
        .join("keystore.key");
    if path.exists() {
        let bytes = fs::read(&path)?;
        if bytes.len() != crypto::KEY_LEN {
            bail!("the messenger keystore is corrupt");
        }
        let mut key = [0u8; crypto::KEY_LEN];
        key.copy_from_slice(&bytes);
        return Ok(key);
    }
    let key = crypto::random_key();
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, key)?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
    fs::rename(&temporary, &path)?;
    Ok(key)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MessengerAccount {
    pub account_id: String,
    /// Hex of the Ed25519 identity key: the name peers see in an invite.
    pub user_id: String,
    pub identity_pk: String,
    pub identity_public: String,
    pub signed_prekey: String,
    pub prekey_signature: String,
    pub display_name: String,
    #[serde(skip)]
    pub avatar: Option<Vec<u8>>,
    pub onion_address: String,
    pub created_at: i64,
    pub active: bool,
    pub pro: bool,
    pub chunk_size: u32,
}

pub struct AccountSecrets {
    pub identity: IdentityKeys,
    pub signed_prekey: x25519_dalek::StaticSecret,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OneTimePrekey {
    pub public: String,
    pub secret: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MessengerContact {
    pub user_id: String,
    pub display_name: String,
    #[serde(default)]
    pub identity_pk: String,
    #[serde(default)]
    pub signed_prekey: String,
    pub onion_address: String,
    pub verified: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationSummary {
    pub conversation_id: String,
    /// `direct` or `group`.
    pub kind: String,
    /// Peer user id for a direct conversation, group id for a group.
    pub target: String,
    pub unread: i64,
    pub updated_at: i64,
    pub last_kind: String,
    pub last_text: String,
    pub last_media: String,
}

/// A scanned invite waiting for an explicit answer on this device.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContactRequestRow {
    pub user_id: String,
    pub display_name: String,
    pub onion_address: String,
    /// The exact invite that arrived, so an acceptance can be returned intact.
    pub invite_json: String,
    /// `incoming` when they scanned our QR, `outgoing` when we scanned theirs.
    pub direction: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageStatus {
    Pending,
    Sent,
    Delivered,
    Read,
    Failed,
}

impl MessageStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            MessageStatus::Pending => "pending",
            MessageStatus::Sent => "sent",
            MessageStatus::Delivered => "delivered",
            MessageStatus::Read => "read",
            MessageStatus::Failed => "failed",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "sent" => MessageStatus::Sent,
            "delivered" => MessageStatus::Delivered,
            "read" => MessageStatus::Read,
            "failed" => MessageStatus::Failed,
            _ => MessageStatus::Pending,
        }
    }
}

pub struct NewMessage {
    pub id: String,
    pub conversation_id: String,
    pub account_id: String,
    pub direction: String,
    pub from_user_id: String,
    pub to_user_id: String,
    pub ts: i64,
    pub kind: String,
    pub text: Option<String>,
    pub reply_to: Option<String>,
    pub media_json: Option<String>,
    pub status: MessageStatus,
    pub ratchet_header: Option<String>,
    pub ciphertext: Option<String>,
    pub signature: Option<String>,
    pub delivered_to: u32,
    pub total_members: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageRecord {
    pub id: String,
    pub conversation_id: String,
    pub account_id: String,
    pub direction: String,
    pub from_user_id: String,
    pub to_user_id: String,
    pub ts: i64,
    pub kind: String,
    pub text: Option<String>,
    pub reply_to: Option<String>,
    pub edited: bool,
    pub edit_ts: Option<i64>,
    pub media_json: Option<String>,
    pub status: MessageStatus,
    pub delivered_to: u32,
    pub total_members: u32,
}

#[derive(Clone)]
pub struct MessengerStore {
    conn: Arc<Mutex<Connection>>,
    master: Arc<Key>,
    app_dir: PathBuf,
}

impl MessengerStore {
    pub fn open(database: &Path, app_dir: &Path) -> Result<Self> {
        if let Some(parent) = database.parent() {
            fs::create_dir_all(parent)?;
            fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
        }
        let conn = Connection::open(database)?;
        fs::set_permissions(database, fs::Permissions::from_mode(0o600))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "busy_timeout", 5_000)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            master: Arc::new(load_or_create_master_key(database)?),
            app_dir: app_dir.to_path_buf(),
        })
    }

    /// Every attachment is written under the app directory only, so a peer
    /// cannot choose a filesystem path on our machine.
    pub fn media_dir(&self) -> PathBuf {
        self.app_dir.join("store/messenger/media")
    }

    pub fn seal(&self, plaintext: &[u8], aad: &str) -> Vec<u8> {
        crypto::seal(&self.master, plaintext, aad.as_bytes())
    }

    pub fn open_sealed(&self, sealed: &[u8], aad: &str) -> Result<Vec<u8>> {
        crypto::open(&self.master, sealed, aad.as_bytes())
    }

    // -----------------------------------------------------------------------
    // Accounts
    // -----------------------------------------------------------------------

    pub fn create_account(&self, display_name: &str, pro: bool) -> Result<MessengerAccount> {
        let display_name = display_name.trim();
        if display_name.is_empty() {
            bail!("a messenger account needs a display name");
        }
        let existing: i64 = self.conn.lock().unwrap().query_row(
            "SELECT COUNT(*) FROM messenger_accounts",
            [],
            |row| row.get(0),
        )?;
        if existing > 0 && existing as usize >= FREE_ACCOUNT_LIMIT && !self.any_pro()? {
            bail!(
                "this installation already has {FREE_ACCOUNT_LIMIT} messenger accounts; remove one or enable the local Pro tier"
            );
        }
        let keys = IdentityKeys::generate();
        let account_id = crypto::random_id();
        let signed_prekey = x25519_dalek::StaticSecret::from(crypto::random_key());
        let signed_prekey_public = x25519_dalek::PublicKey::from(&signed_prekey)
            .as_bytes()
            .to_vec();
        let signature = crypto::sign(
            &keys.signing(),
            &[
                b"gnomeai-messenger-signed-prekey-v1".as_slice(),
                &signed_prekey_public,
            ]
            .concat(),
        );
        let signed_prekey_secret =
            self.seal(&signed_prekey.to_bytes(), &format!("{account_id}/spk"));
        let identity_seed = self.seal(&keys.signing_seed, &format!("{account_id}/signing"));
        let dh_seed = self.seal(&keys.dh_seed, &format!("{account_id}/dh"));
        let created_at = now_ms();
        let active = self.active_account()?.is_none();
        let account = MessengerAccount {
            account_id: account_id.clone(),
            user_id: keys.user_id(),
            identity_pk: hex::encode(keys.dh_public()),
            identity_public: hex::encode(keys.signing_public().to_bytes()),
            signed_prekey: hex::encode(&signed_prekey_public),
            prekey_signature: signature,
            display_name: display_name.to_string(),
            avatar: None,
            onion_address: String::new(),
            created_at,
            active,
            pro,
            chunk_size: super::media::DEFAULT_CHUNK_SIZE,
        };
        self.conn.lock().unwrap().execute(
            "INSERT INTO messenger_accounts (
                account_id, user_id, display_name, onion_address, identity_pk,
                identity_public, identity_seed, dh_seed, signed_prekey,
                signed_prekey_secret, prekey_signature, created_at, active, pro, chunk_size
             ) VALUES (?1,?2,?3,'',?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            params![
                account.account_id,
                account.user_id,
                account.display_name,
                account.identity_pk,
                account.identity_public,
                identity_seed,
                dh_seed,
                account.signed_prekey,
                signed_prekey_secret,
                account.prekey_signature,
                created_at,
                i64::from(active),
                i64::from(pro),
                account.chunk_size,
            ],
        )?;
        if active {
            self.set_active(&account_id)?;
        }
        Ok(account)
    }

    fn any_pro(&self) -> Result<bool> {
        let count: i64 = self.conn.lock().unwrap().query_row(
            "SELECT COUNT(*) FROM messenger_accounts WHERE pro=1",
            [],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn accounts(&self) -> Result<Vec<MessengerAccount>> {
        let conn = self.conn.lock().unwrap();
        let mut statement = conn.prepare(
            "SELECT account_id, user_id, display_name, avatar, onion_address,
                    identity_pk, identity_public, signed_prekey, prekey_signature,
                    created_at, active, pro, chunk_size
             FROM messenger_accounts ORDER BY created_at",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(MessengerAccount {
                account_id: row.get(0)?,
                user_id: row.get(1)?,
                display_name: row.get(2)?,
                avatar: row.get(3)?,
                onion_address: row.get(4)?,
                identity_pk: row.get(5)?,
                identity_public: row.get(6)?,
                signed_prekey: row.get(7)?,
                prekey_signature: row.get(8)?,
                created_at: row.get(9)?,
                active: row.get::<_, i64>(10)? != 0,
                pro: row.get::<_, i64>(11)? != 0,
                chunk_size: row.get::<_, i64>(12)? as u32,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn account(&self, account_id: &str) -> Result<Option<MessengerAccount>> {
        Ok(self
            .accounts()?
            .into_iter()
            .find(|account| account.account_id == account_id))
    }

    pub fn active_account(&self) -> Result<Option<MessengerAccount>> {
        Ok(self.accounts()?.into_iter().find(|account| account.active))
    }

    pub fn set_active(&self, account_id: &str) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let transaction = conn.transaction()?;
        transaction.execute("UPDATE messenger_accounts SET active=0", [])?;
        transaction.execute(
            "UPDATE messenger_accounts SET active=1 WHERE account_id=?1",
            [account_id],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn delete_account(&self, account_id: &str) -> Result<()> {
        {
            let mut conn = self.conn.lock().unwrap();
            let transaction = conn.transaction()?;
            for table in [
                "messenger_messages",
                "messenger_conversations",
                "messenger_contacts",
                "messenger_ratchets",
                "messenger_groups",
                "messenger_sender_keys",
                "messenger_outbox",
                "messenger_transfers",
            ] {
                transaction.execute(
                    &format!("DELETE FROM {table} WHERE account_id=?1"),
                    [account_id],
                )?;
            }
            transaction.execute(
                "DELETE FROM messenger_accounts WHERE account_id=?1",
                [account_id],
            )?;
            transaction.commit()?;
        }
        if self.active_account()?.is_none() {
            if let Some(next) = self.accounts()?.first() {
                let next = next.account_id.clone();
                self.set_active(&next)?;
            }
        }
        Ok(())
    }

    pub fn set_onion(&self, account_id: &str, onion: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE messenger_accounts SET onion_address=?2 WHERE account_id=?1",
            params![account_id, onion],
        )?;
        Ok(())
    }

    pub fn set_profile(
        &self,
        account_id: &str,
        display_name: &str,
        avatar: Option<Vec<u8>>,
    ) -> Result<()> {
        let display_name = display_name.trim();
        if display_name.is_empty() {
            bail!("a display name cannot be empty");
        }
        self.conn.lock().unwrap().execute(
            "UPDATE messenger_accounts SET display_name=?2, avatar=?3 WHERE account_id=?1",
            params![account_id, display_name, avatar],
        )?;
        Ok(())
    }

    /// The Pro tier is purely local: it only raises this installation's file
    /// ceiling and the account limit. No server is asked and none exists.
    pub fn set_tier(&self, account_id: &str, pro: bool, chunk_size: u32) -> Result<()> {
        let chunk_size = chunk_size.clamp(64 * 1024, super::media::MAX_CHUNK_SIZE);
        self.conn.lock().unwrap().execute(
            "UPDATE messenger_accounts SET pro=?2, chunk_size=?3 WHERE account_id=?1",
            params![account_id, i64::from(pro), i64::from(chunk_size)],
        )?;
        Ok(())
    }

    /// Identity secrets, unsealed on demand and never rendered anywhere.
    pub fn identity_secrets(&self, account_id: &str) -> Result<AccountSecrets> {
        let (signing_seed, dh_seed, signed_prekey): (Vec<u8>, Vec<u8>, Vec<u8>) = self
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT identity_seed, dh_seed, signed_prekey_secret
                 FROM messenger_accounts WHERE account_id=?1",
                [account_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .context("the messenger account does not exist")?;
        let signing_seed = self.open_sealed(&signing_seed, &format!("{account_id}/signing"))?;
        let dh_seed = self.open_sealed(&dh_seed, &format!("{account_id}/dh"))?;
        let signed_prekey = self.open_sealed(&signed_prekey, &format!("{account_id}/spk"))?;
        Ok(AccountSecrets {
            identity: IdentityKeys {
                signing_seed: signing_seed
                    .as_slice()
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("the stored identity seed is malformed"))?,
                dh_seed: dh_seed
                    .as_slice()
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("the stored DH seed is malformed"))?,
            },
            signed_prekey: x25519_dalek::StaticSecret::from(
                <[u8; 32]>::try_from(signed_prekey.as_slice())
                    .map_err(|_| anyhow::anyhow!("the stored signed prekey is malformed"))?,
            ),
        })
    }
}

impl MessengerStore {
    // -----------------------------------------------------------------------
    // One-time prekeys
    // -----------------------------------------------------------------------

    /// The pool lives only on this device and is topped up lazily.
    ///
    /// A key may appear in more than one bundle; what is used exactly once is
    /// the *consumption*, in `consume_one_time_prekey`. That is the property
    /// that matters: a replayed preamble finds its key already gone, so it
    /// cannot open a second session.
    pub fn ensure_one_time_prekeys(&self, account_id: &str, wanted: usize) -> Result<()> {
        let mut pool = self.one_time_prekeys(account_id)?;
        if pool.len() >= wanted {
            return Ok(());
        }
        while pool.len() < wanted {
            let secret = x25519_dalek::StaticSecret::from(crypto::random_key());
            pool.push(OneTimePrekey {
                public: hex::encode(x25519_dalek::PublicKey::from(&secret).as_bytes()),
                secret: hex::encode(secret.to_bytes()),
            });
        }
        self.save_one_time_prekeys(account_id, &pool)
    }

    fn one_time_prekeys(&self, account_id: &str) -> Result<Vec<OneTimePrekey>> {
        // A brand-new account has the schema default `X''`, and a keystore that
        // was rotated cannot open an older pool. Both mean "no prekeys yet":
        // the pool is regenerated, and nothing else is affected.
        let sealed: Vec<u8> = self
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT one_time_prekeys FROM messenger_accounts WHERE account_id=?1",
                [account_id],
                |row| row.get(0),
            )
            .context("the messenger account does not exist")?;
        match self.open_sealed(&sealed, &format!("{account_id}/opk")) {
            Ok(plaintext) => Ok(serde_json::from_slice(&plaintext).unwrap_or_default()),
            Err(_) => Ok(Vec::new()),
        }
    }

    fn save_one_time_prekeys(&self, account_id: &str, pool: &[OneTimePrekey]) -> Result<()> {
        let encoded = serde_json::to_vec(pool)?;
        let sealed = self.seal(&encoded, &format!("{account_id}/opk"));
        self.conn.lock().unwrap().execute(
            "UPDATE messenger_accounts SET one_time_prekeys=?2 WHERE account_id=?1",
            params![account_id, sealed],
        )?;
        Ok(())
    }

    /// Offer one prekey in a bundle, leaving it in the pool.
    ///
    /// The key is spent when a preamble actually arrives and names it — that is
    /// `consume_one_time_prekey`. Removing it here looked tidy but broke X3DH
    /// outright: a bundle is an *offer*, and most offers are never used (the peer
    /// is offline, the message is queued, the user changes their mind). Handing
    /// out the key and immediately forgetting its secret meant the honest
    /// preamble that did come back found nothing to consume, and every such
    /// message failed with "the one-time prekey was already used".
    ///
    /// Handing the same key out again is safe: X3DH derives a fresh session from
    /// it each time, and the first preamble to consume it wins.
    pub fn take_one_time_prekey(&self, account_id: &str) -> Result<Option<OneTimePrekey>> {
        let pool = self.one_time_prekeys(account_id)?;
        // The tail is handed out first so the pool is drained in a stable order.
        Ok(pool.last().cloned())
    }

    /// Consume a named prekey, used by an X3DH responder. A prekey that was
    /// never handed out is simply absent, which the caller treats as a stale
    /// preamble rather than an error.
    pub fn consume_one_time_prekey(
        &self,
        account_id: &str,
        public: &str,
    ) -> Result<Option<OneTimePrekey>> {
        let mut pool = self.one_time_prekeys(account_id)?;
        let Some(index) = pool.iter().position(|candidate| candidate.public == public) else {
            return Ok(None);
        };
        let taken = pool.remove(index);
        self.save_one_time_prekeys(account_id, &pool)?;
        Ok(Some(taken))
    }

    pub fn one_time_prekey_count(&self, account_id: &str) -> Result<usize> {
        Ok(self.one_time_prekeys(account_id)?.len())
    }
}

impl MessengerStore {
    // -----------------------------------------------------------------------
    // Contacts and conversations
    // -----------------------------------------------------------------------

    pub fn upsert_contact(&self, account_id: &str, contact: &MessengerContact) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO messenger_contacts
                (account_id, user_id, display_name, identity_pk, signed_prekey, onion_address, verified, created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
             ON CONFLICT(account_id,user_id) DO UPDATE SET
                display_name=excluded.display_name,
                identity_pk=excluded.identity_pk,
                signed_prekey=excluded.signed_prekey,
                onion_address=excluded.onion_address,
                verified=excluded.verified",
            params![
                account_id,
                contact.user_id,
                contact.display_name,
                contact.identity_pk,
                contact.signed_prekey,
                contact.onion_address,
                i64::from(contact.verified),
                now_ms()
            ],
        )?;
        self.direct_conversation(account_id, &contact.user_id)?;
        Ok(())
    }

    pub fn contact(&self, account_id: &str, user_id: &str) -> Result<Option<MessengerContact>> {
        Ok(self
            .contacts(account_id)?
            .into_iter()
            .find(|contact| contact.user_id == user_id))
    }

    pub fn contacts(&self, account_id: &str) -> Result<Vec<MessengerContact>> {
        let conn = self.conn.lock().unwrap();
        let mut statement = conn.prepare(
            "SELECT user_id, display_name, identity_pk, signed_prekey, onion_address, verified
             FROM messenger_contacts WHERE account_id=?1 ORDER BY display_name",
        )?;
        let rows = statement.query_map([account_id], |row| {
            Ok(MessengerContact {
                user_id: row.get(0)?,
                display_name: row.get(1)?,
                identity_pk: row.get(2)?,
                signed_prekey: row.get(3)?,
                onion_address: row.get(4)?,
                verified: row.get::<_, i64>(5)? != 0,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// A pending request, plus the exact invite that arrived with it, so an
    /// acceptance can be signed without guessing which onion was scanned.
    pub fn upsert_contact_request(
        &self,
        account_id: &str,
        contact: &MessengerContact,
        invite_json: &str,
        direction: &str,
    ) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO messenger_contact_requests
                (account_id, user_id, display_name, identity_pk, signed_prekey,
                 onion_address, invite_json, direction, created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
             ON CONFLICT(account_id,user_id) DO UPDATE SET
                display_name=excluded.display_name,
                identity_pk=excluded.identity_pk,
                signed_prekey=excluded.signed_prekey,
                onion_address=excluded.onion_address,
                invite_json=excluded.invite_json,
                direction=excluded.direction",
            params![
                account_id,
                contact.user_id,
                contact.display_name,
                contact.identity_pk,
                contact.signed_prekey,
                contact.onion_address,
                invite_json,
                direction,
                now_ms()
            ],
        )?;
        Ok(())
    }

    /// Every request still waiting for this device's answer.
    pub fn contact_requests(&self, account_id: &str) -> Result<Vec<ContactRequestRow>> {
        let conn = self.conn.lock().unwrap();
        let mut statement = conn.prepare(
            "SELECT user_id, display_name, onion_address, invite_json, direction, created_at
             FROM messenger_contact_requests WHERE account_id=?1 ORDER BY created_at",
        )?;
        let rows = statement.query_map([account_id], |row| {
            Ok(ContactRequestRow {
                user_id: row.get(0)?,
                display_name: row.get(1)?,
                onion_address: row.get(2)?,
                invite_json: row.get(3)?,
                direction: row.get(4)?,
                created_at: row.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn remove_contact_request(&self, account_id: &str, user_id: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "DELETE FROM messenger_contact_requests WHERE account_id=?1 AND user_id=?2",
            params![account_id, user_id],
        )?;
        Ok(())
    }

    /// Drop the transcript note that announced a contact request.
    ///
    /// The note exists so the request is visible wherever the user happens to
    /// be; once the request is answered the note would only claim that somebody
    /// is still waiting. The id matches the one `note_contact_request` inserts.
    pub fn remove_contact_request_note(&self, account_id: &str, user_id: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "DELETE FROM messenger_messages WHERE id=?1 AND account_id=?2",
            params![format!("request:{account_id}:{user_id}"), account_id],
        )?;
        Ok(())
    }

    /// Conversations are created lazily, so an untouched contact stays out of
    /// the conversation list until there is something to show.
    pub fn direct_conversation(&self, account_id: &str, peer_user_id: &str) -> Result<String> {
        let id = format!("dm:{account_id}:{peer_user_id}");
        self.conn.lock().unwrap().execute(
            "INSERT INTO messenger_conversations
                (conversation_id, account_id, kind, peer_user_id, group_id, unread, updated_at)
             VALUES (?1,?2,'direct',?3,'',0,?4)
             ON CONFLICT(account_id,kind,peer_user_id) DO NOTHING",
            params![id, account_id, peer_user_id, now_ms()],
        )?;
        Ok(id)
    }

    pub fn group_conversation(&self, account_id: &str, group_id: &str) -> Result<String> {
        let id = format!("grp:{account_id}:{group_id}");
        self.conn.lock().unwrap().execute(
            "INSERT INTO messenger_conversations
                (conversation_id, account_id, kind, peer_user_id, group_id, unread, updated_at)
             VALUES (?1,?2,'group',?3,?3,0,?4)
             ON CONFLICT(conversation_id) DO NOTHING",
            params![id, account_id, group_id, now_ms()],
        )?;
        Ok(id)
    }

    pub fn conversation_id_for(
        &self,
        account_id: &str,
        kind: &str,
        target: &str,
    ) -> Result<String> {
        if kind == "group" {
            self.group_conversation(account_id, target)
        } else {
            self.direct_conversation(account_id, target)
        }
    }

    pub fn conversations(&self, account_id: &str) -> Result<Vec<ConversationSummary>> {
        let conn = self.conn.lock().unwrap();
        let mut statement = conn.prepare(
            "SELECT c.conversation_id, c.kind, c.peer_user_id, c.group_id, c.unread, c.updated_at,
                    (SELECT kind FROM messenger_messages m WHERE m.conversation_id=c.conversation_id
                     ORDER BY m.ts DESC, m.id DESC LIMIT 1),
                    (SELECT COALESCE(text,'') FROM messenger_messages m WHERE m.conversation_id=c.conversation_id
                     ORDER BY m.ts DESC, m.id DESC LIMIT 1),
                    (SELECT COALESCE(media_json,'') FROM messenger_messages m WHERE m.conversation_id=c.conversation_id
                     ORDER BY m.ts DESC, m.id DESC LIMIT 1)
             FROM messenger_conversations c WHERE c.account_id=?1
             ORDER BY c.updated_at DESC",
        )?;
        let rows = statement.query_map([account_id], |row| {
            let kind: String = row.get(1)?;
            let peer_user_id: String = row.get(2)?;
            let group_id: String = row.get(3)?;
            Ok(ConversationSummary {
                conversation_id: row.get(0)?,
                target: if kind == "group" {
                    group_id
                } else {
                    peer_user_id
                },
                kind,
                unread: row.get(4)?,
                updated_at: row.get(5)?,
                last_kind: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
                last_text: row.get::<_, Option<String>>(7)?.unwrap_or_default(),
                last_media: row.get::<_, Option<String>>(8)?.unwrap_or_default(),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn clear_unread(&self, account_id: &str, conversation_id: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE messenger_conversations SET unread=0 WHERE account_id=?1 AND conversation_id=?2",
            params![account_id, conversation_id],
        )?;
        Ok(())
    }
}

impl MessengerStore {
    // -----------------------------------------------------------------------
    // Messages
    // -----------------------------------------------------------------------

    pub fn insert_message(&self, message: &NewMessage) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let transaction = conn.transaction()?;
        transaction.execute(
            "INSERT OR REPLACE INTO messenger_messages (
                id, conversation_id, account_id, direction, from_user_id, to_user_id, ts,
                kind, text, reply_to, edited, edit_ts, media_json, status,
                ratchet_header, ciphertext, signature, delivered_to, total_members
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,0,NULL,?11,?12,?13,?14,?15,?16,?17)",
            params![
                message.id,
                message.conversation_id,
                message.account_id,
                message.direction,
                message.from_user_id,
                message.to_user_id,
                message.ts,
                message.kind,
                message.text,
                message.reply_to,
                message.media_json,
                message.status.as_str(),
                message.ratchet_header,
                message.ciphertext,
                message.signature,
                message.delivered_to as i64,
                message.total_members as i64,
            ],
        )?;
        let unread = if message.direction == "incoming" {
            "unread=unread+1,"
        } else {
            ""
        };
        transaction.execute(
            &format!(
                "UPDATE messenger_conversations SET {unread} updated_at=?3
                 WHERE conversation_id=?1 AND account_id=?2"
            ),
            params![message.conversation_id, message.account_id, message.ts],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn message(&self, id: &str) -> Result<Option<MessageRecord>> {
        Ok(self
            .messages_where("id=?1", params![id])?
            .into_iter()
            .next())
    }

    fn messages_where(
        &self,
        clause: &str,
        values: impl rusqlite::Params,
    ) -> Result<Vec<MessageRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut statement = conn.prepare(&format!(
            "SELECT id, conversation_id, account_id, direction, from_user_id, to_user_id, ts,
                    kind, text, reply_to, edited, edit_ts, media_json, status,
                    delivered_to, total_members
             FROM messenger_messages WHERE {clause} ORDER BY ts, id LIMIT 5000"
        ))?;
        let rows = statement.query_map(values, |row| {
            Ok(MessageRecord {
                id: row.get(0)?,
                conversation_id: row.get(1)?,
                account_id: row.get(2)?,
                direction: row.get(3)?,
                from_user_id: row.get(4)?,
                to_user_id: row.get(5)?,
                ts: row.get(6)?,
                kind: row.get(7)?,
                text: row.get(8)?,
                reply_to: row.get(9)?,
                edited: row.get::<_, i64>(10)? != 0,
                edit_ts: row.get(11)?,
                media_json: row.get(12)?,
                status: MessageStatus::parse(&row.get::<_, String>(13)?),
                delivered_to: row.get::<_, i64>(14)? as u32,
                total_members: row.get::<_, i64>(15)? as u32,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn messages(&self, account_id: &str, conversation_id: &str) -> Result<Vec<MessageRecord>> {
        self.messages_where(
            "account_id=?1 AND conversation_id=?2",
            params![account_id, conversation_id],
        )
    }

    /// Apply an edit in place, keeping the previous version for a local audit.
    /// Only text is editable in v1: media is delete-and-resend.
    pub fn edit_message(
        &self,
        account_id: &str,
        id: &str,
        new_text: &str,
    ) -> Result<Option<MessageRecord>> {
        let Some(existing) = self.message(id)? else {
            return Ok(None);
        };
        if existing.account_id != account_id {
            bail!("that message belongs to another account");
        }
        if existing.kind != "text" && existing.kind != "edit" {
            bail!("only text messages can be edited; resend the attachment instead");
        }
        if existing.direction != "outgoing" {
            bail!("only your own messages can be edited");
        }
        let previous = existing.text.clone().unwrap_or_default();
        let versions: Vec<serde_json::Value> = self
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT versions_json FROM messenger_messages WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()?
            .and_then(|encoded: String| serde_json::from_str(&encoded).ok())
            .unwrap_or_default();
        let mut versions = versions;
        versions.push(serde_json::json!({"text": previous, "ts": existing.ts}));
        let ts = now_ms();
        self.conn.lock().unwrap().execute(
            "UPDATE messenger_messages
             SET text=?2, edited=1, edit_ts=?3, versions_json=?4
             WHERE id=?1",
            params![id, new_text, ts, serde_json::to_string(&versions)?],
        )?;
        self.message(id)
    }

    pub fn update_status(&self, id: &str, status: MessageStatus) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE messenger_messages SET status=?2 WHERE id=?1",
            params![id, status.as_str()],
        )?;
        Ok(())
    }

    /// Receipts only ever move forward. A `delivered` that arrives after a
    /// `read` must not roll the ticker back.
    pub fn raise_status(&self, id: &str, status: MessageStatus) -> Result<bool> {
        let Some(existing) = self.message(id)? else {
            return Ok(false);
        };
        let rank = |value: MessageStatus| match value {
            MessageStatus::Failed => 0,
            MessageStatus::Pending => 1,
            MessageStatus::Sent => 2,
            MessageStatus::Delivered => 3,
            MessageStatus::Read => 4,
        };
        if rank(status) <= rank(existing.status) {
            return Ok(false);
        }
        self.update_status(id, status)?;
        Ok(true)
    }

    pub fn set_delivery(&self, id: &str, delivered_to: u32, total_members: u32) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE messenger_messages SET delivered_to=?2, total_members=?3 WHERE id=?1",
            params![id, delivered_to as i64, total_members as i64],
        )?;
        Ok(())
    }

    /// Messages that still need a delivery attempt, oldest first.
    pub fn pending(&self, account_id: &str) -> Result<Vec<MessageRecord>> {
        self.messages_where(
            "account_id=?1 AND direction='outgoing' AND status IN ('pending','failed')",
            params![account_id],
        )
    }

    /// An edited message is re-sent as an `edit` body, so it goes back in the
    /// queue without pretending to be new.
    pub fn mark_pending(&self, id: &str) -> Result<()> {
        self.update_status(id, MessageStatus::Pending)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GroupRecord {
    pub group_id: String,
    pub account_id: String,
    pub name: String,
    pub created_by: String,
    pub created_at: i64,
    pub members: Vec<super::wire::GroupMemberWire>,
    pub admins: Vec<String>,
    pub revoked: bool,
}

impl GroupRecord {
    pub fn member(&self, user_id: &str) -> Option<&super::wire::GroupMemberWire> {
        self.members.iter().find(|member| member.user_id == user_id)
    }

    pub fn is_admin(&self, user_id: &str) -> bool {
        self.admins.iter().any(|admin| admin == user_id)
    }

    /// Everyone except us: that is the fan-out set.
    pub fn peers(&self, own_user_id: &str) -> Vec<super::wire::GroupMemberWire> {
        self.members
            .iter()
            .filter(|member| member.user_id != own_user_id && !member.onion.is_empty())
            .cloned()
            .collect()
    }
}

impl MessengerStore {
    // -----------------------------------------------------------------------
    // Groups
    // -----------------------------------------------------------------------

    pub fn create_group(
        &self,
        account_id: &str,
        name: &str,
        created_by: &str,
        members: Vec<super::wire::GroupMemberWire>,
    ) -> Result<GroupRecord> {
        let name = name.trim();
        if name.is_empty() {
            bail!("a group needs a name");
        }
        if members.len() > MAX_GROUP_MEMBERS {
            bail!("a group is limited to {MAX_GROUP_MEMBERS} members");
        }
        let group = GroupRecord {
            group_id: crypto::random_id(),
            account_id: account_id.to_string(),
            name: name.to_string(),
            created_by: created_by.to_string(),
            created_at: now_ms(),
            admins: vec![created_by.to_string()],
            members,
            revoked: false,
        };
        self.save_group(&group)?;
        self.group_conversation(account_id, &group.group_id)?;
        Ok(group)
    }

    pub fn save_group(&self, group: &GroupRecord) -> Result<()> {
        if group.members.len() > MAX_GROUP_MEMBERS {
            bail!("a group is limited to {MAX_GROUP_MEMBERS} members");
        }
        self.conn.lock().unwrap().execute(
            "INSERT INTO messenger_groups
                (group_id, account_id, name, created_by, created_at, members, admins, revoked)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
             ON CONFLICT(group_id) DO UPDATE SET
                name=excluded.name,
                members=excluded.members,
                admins=excluded.admins,
                revoked=excluded.revoked",
            params![
                group.group_id,
                group.account_id,
                group.name,
                group.created_by,
                group.created_at,
                serde_json::to_string(&group.members)?,
                serde_json::to_string(&group.admins)?,
                i64::from(group.revoked),
            ],
        )?;
        Ok(())
    }

    pub fn group(&self, account_id: &str, group_id: &str) -> Result<Option<GroupRecord>> {
        Ok(self
            .groups(account_id)?
            .into_iter()
            .find(|group| group.group_id == group_id))
    }

    pub fn groups(&self, account_id: &str) -> Result<Vec<GroupRecord>> {
        let conn = self.conn.lock().unwrap();
        let mut statement = conn.prepare(
            "SELECT group_id, account_id, name, created_by, created_at, members, admins, revoked
             FROM messenger_groups WHERE account_id=?1 ORDER BY created_at DESC",
        )?;
        let rows = statement.query_map([account_id], |row| {
            let members: String = row.get(5)?;
            let admins: String = row.get(6)?;
            Ok(GroupRecord {
                group_id: row.get(0)?,
                account_id: row.get(1)?,
                name: row.get(2)?,
                created_by: row.get(3)?,
                created_at: row.get(4)?,
                members: serde_json::from_str(&members).unwrap_or_default(),
                admins: serde_json::from_str(&admins).unwrap_or_default(),
                revoked: row.get::<_, i64>(7)? != 0,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // -----------------------------------------------------------------------
    // Sender keys
    // -----------------------------------------------------------------------

    pub fn save_sender_chain(
        &self,
        account_id: &str,
        group_id: &str,
        sender_id: &str,
        chain: &crypto::SenderChain,
    ) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO messenger_sender_keys (account_id, group_id, sender_id, chain, updated_at)
             VALUES (?1,?2,?3,?4,?5)
             ON CONFLICT(account_id,group_id,sender_id) DO UPDATE SET
                chain=excluded.chain, updated_at=excluded.updated_at",
            params![account_id, group_id, sender_id, chain.to_bytes(), now_ms()],
        )?;
        Ok(())
    }

    pub fn sender_chain(
        &self,
        account_id: &str,
        group_id: &str,
        sender_id: &str,
    ) -> Result<Option<crypto::SenderChain>> {
        let encoded: Option<Vec<u8>> = self
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT chain FROM messenger_sender_keys
                 WHERE account_id=?1 AND group_id=?2 AND sender_id=?3",
                params![account_id, group_id, sender_id],
                |row| row.get(0),
            )
            .optional()?;
        encoded.map(crypto::SenderChain::from_bytes).transpose()
    }

    pub fn drop_sender_chains(&self, account_id: &str, group_id: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "DELETE FROM messenger_sender_keys WHERE account_id=?1 AND group_id=?2",
            params![account_id, group_id],
        )?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Ratchet state
    // -----------------------------------------------------------------------

    pub fn save_ratchet(
        &self,
        account_id: &str,
        peer_user_id: &str,
        state: &crypto::RatchetState,
    ) -> Result<()> {
        let encoded = serde_json::to_vec(state)?;
        let sealed = self.seal(&encoded, &format!("{account_id}/ratchet/{peer_user_id}"));
        self.conn.lock().unwrap().execute(
            "INSERT INTO messenger_ratchets (account_id, peer_user_id, state, updated_at)
             VALUES (?1,?2,?3,?4)
             ON CONFLICT(account_id,peer_user_id) DO UPDATE SET
                state=excluded.state, updated_at=excluded.updated_at",
            params![account_id, peer_user_id, sealed, now_ms()],
        )?;
        Ok(())
    }

    pub fn ratchet(
        &self,
        account_id: &str,
        peer_user_id: &str,
    ) -> Result<Option<crypto::RatchetState>> {
        let sealed: Option<Vec<u8>> = self
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT state FROM messenger_ratchets WHERE account_id=?1 AND peer_user_id=?2",
                params![account_id, peer_user_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(sealed) = sealed else {
            return Ok(None);
        };
        let plaintext =
            self.open_sealed(&sealed, &format!("{account_id}/ratchet/{peer_user_id}"))?;
        Ok(serde_json::from_slice(&plaintext).ok())
    }

    pub fn drop_ratchet(&self, account_id: &str, peer_user_id: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "DELETE FROM messenger_ratchets WHERE account_id=?1 AND peer_user_id=?2",
            params![account_id, peer_user_id],
        )?;
        Ok(())
    }
}

/// A queued delivery attempt, sealed until it is flushed.
#[derive(Debug, Clone)]
pub struct OutboxItem {
    pub id: String,
    pub account_id: String,
    pub message_id: String,
    pub peer_user_id: String,
    pub request_json: String,
    pub attempts: i64,
}

impl MessengerStore {
    // -----------------------------------------------------------------------
    // Outbox: the offline queue. Nothing leaves the device until a peer is
    // reachable, and a restart does not lose the queue.
    // -----------------------------------------------------------------------

    pub fn enqueue(
        &self,
        account_id: &str,
        message_id: &str,
        peer_user_id: &str,
        request_json: &str,
    ) -> Result<()> {
        let id = crypto::random_id();
        let sealed = self.seal(
            request_json.as_bytes(),
            &format!("{account_id}/outbox/{id}"),
        );
        self.conn.lock().unwrap().execute(
            "INSERT INTO messenger_outbox
                (id, account_id, message_id, peer_user_id, payload, attempts, created_at)
             VALUES (?1,?2,?3,?4,?5,0,?6)",
            params![id, account_id, message_id, peer_user_id, sealed, now_ms()],
        )?;
        Ok(())
    }

    pub fn outbox(&self, account_id: &str) -> Result<Vec<OutboxItem>> {
        let rows: Vec<(String, String, String, i64, Vec<u8>, String)> = {
            let conn = self.conn.lock().unwrap();
            let mut statement = conn.prepare(
                "SELECT id, message_id, peer_user_id, payload, attempts, account_id
                 FROM messenger_outbox WHERE account_id=?1 ORDER BY created_at LIMIT 2000",
            )?;
            let mapped = statement.query_map([account_id], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(4)?,
                    row.get(3)?,
                    row.get(5)?,
                ))
            })?;
            mapped.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut items = Vec::with_capacity(rows.len());
        for (id, message_id, peer_user_id, attempts, payload, _account) in rows {
            let plaintext = self.open_sealed(&payload, &format!("{account_id}/outbox/{id}"))?;
            items.push(OutboxItem {
                id,
                account_id: account_id.to_string(),
                message_id,
                peer_user_id,
                request_json: String::from_utf8(plaintext)?,
                attempts,
            });
        }
        Ok(items)
    }

    pub fn outbox_for_peer(&self, account_id: &str, peer_user_id: &str) -> Result<Vec<OutboxItem>> {
        Ok(self
            .outbox(account_id)?
            .into_iter()
            .filter(|item| item.peer_user_id == peer_user_id)
            .collect())
    }

    pub fn dequeue(&self, id: &str) -> Result<()> {
        self.conn
            .lock()
            .unwrap()
            .execute("DELETE FROM messenger_outbox WHERE id=?1", [id])?;
        Ok(())
    }

    pub fn note_attempt(&self, id: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE messenger_outbox SET attempts=attempts+1 WHERE id=?1",
            [id],
        )?;
        Ok(())
    }

    pub fn outbox_len(&self, account_id: &str) -> Result<i64> {
        Ok(self.conn.lock().unwrap().query_row(
            "SELECT COUNT(*) FROM messenger_outbox WHERE account_id=?1",
            [account_id],
            |row| row.get(0),
        )?)
    }

    // -----------------------------------------------------------------------
    // Attachment transfers
    // -----------------------------------------------------------------------

    pub fn start_transfer(
        &self,
        transfer_id: &str,
        account_id: &str,
        peer_user_id: &str,
        direction: &str,
        message_id: &str,
        path: &str,
        meta_json: &str,
        key: &Key,
    ) -> Result<()> {
        let sealed = self.seal(key, &format!("{account_id}/transfer/{transfer_id}"));
        self.conn.lock().unwrap().execute(
            "INSERT INTO messenger_transfers
                (transfer_id, account_id, peer_user_id, direction, message_id, path, meta_json, key, received, status, updated_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,0,'active',?9)
             ON CONFLICT(transfer_id) DO UPDATE SET received=excluded.received, status=excluded.status",
            params![transfer_id, account_id, peer_user_id, direction, message_id, path, meta_json, sealed, now_ms()],
        )?;
        Ok(())
    }

    pub fn transfer_key(&self, account_id: &str, transfer_id: &str) -> Result<Option<Key>> {
        let sealed: Option<Vec<u8>> = self
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT key FROM messenger_transfers WHERE transfer_id=?1 AND account_id=?2",
                params![transfer_id, account_id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(sealed) = sealed else {
            return Ok(None);
        };
        let plaintext =
            self.open_sealed(&sealed, &format!("{account_id}/transfer/{transfer_id}"))?;
        Ok(<[u8; crypto::KEY_LEN]>::try_from(plaintext.as_slice()).ok())
    }

    pub fn transfer_meta(&self, account_id: &str, transfer_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT meta_json FROM messenger_transfers WHERE transfer_id=?1 AND account_id=?2",
                params![transfer_id, account_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn set_transfer_progress(
        &self,
        transfer_id: &str,
        received: u32,
        status: &str,
    ) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE messenger_transfers SET received=?2, status=?3, updated_at=?4 WHERE transfer_id=?1",
            params![transfer_id, received as i64, status, now_ms()],
        )?;
        Ok(())
    }
}
