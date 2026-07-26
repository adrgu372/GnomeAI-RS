//! Coding-agent session state — SQLite.
//!
//! Replaces `store/memory.json`. Four things a JSON file cannot give you:
//! resumable sessions, forking, per-file rollback, and non-destructive context
//! compaction.
//!
//! Compaction here is *soft*: old turns are never deleted, only marked as
//! superseded by a summary turn. The context builder reads live turns; the
//! debugger, the audit log and `fork` all still see everything. Destructive
//! compaction saves a few megabytes and costs you the ability to answer "what
//! did it actually do three hours ago".

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use crate::apply_patch;

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

const SCHEMA_VERSION: i32 = 1;

const SCHEMA: &str = r#"
CREATE TABLE sessions (
    id              TEXT PRIMARY KEY,
    workspace       TEXT NOT NULL,
    title           TEXT,
    model           TEXT NOT NULL,
    status          TEXT NOT NULL DEFAULT 'active',
    parent_id       TEXT REFERENCES sessions(id),
    forked_at_seq   INTEGER,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);

CREATE TABLE turns (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id      TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    seq             INTEGER NOT NULL,
    role            TEXT NOT NULL,
    content         TEXT NOT NULL,
    tokens          INTEGER NOT NULL DEFAULT 0,
    is_summary      INTEGER NOT NULL DEFAULT 0,
    -- Non-null means this turn has been folded into a summary and is no
    -- longer sent to the model. The row stays for audit and fork.
    superseded_by   INTEGER REFERENCES turns(id),
    -- Pinned turns are never compacted: the system prompt and the original
    -- task. An agent that summarises away its own goal drifts within a few
    -- rounds and nobody can work out why.
    pinned          INTEGER NOT NULL DEFAULT 0,
    created_at      INTEGER NOT NULL,
    UNIQUE(session_id, seq)
);
CREATE INDEX idx_turns_live ON turns(session_id, superseded_by, seq);

CREATE TABLE tool_calls (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    turn_id         INTEGER NOT NULL REFERENCES turns(id) ON DELETE CASCADE,
    call_id         TEXT NOT NULL,
    name            TEXT NOT NULL,
    arguments       TEXT NOT NULL,
    result          TEXT,
    exit_code       INTEGER,
    duration_ms     INTEGER,
    approved        INTEGER,
    created_at      INTEGER NOT NULL
);
CREATE INDEX idx_tool_calls_turn ON tool_calls(turn_id);

CREATE TABLE patches (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id      TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    turn_id         INTEGER REFERENCES turns(id),
    path            TEXT NOT NULL,
    -- NULL before = file did not exist. NULL after = file was deleted.
    before          BLOB,
    after           BLOB,
    diff            TEXT NOT NULL,
    applied_at      INTEGER NOT NULL,
    reverted_at     INTEGER
);
CREATE INDEX idx_patches_session ON patches(session_id, id);
"#;

// ---------------------------------------------------------------------------
// Store
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("cannot protect {}", path.display()))?;

        // WAL lets readers proceed while a write is in flight. Without it the
        // web UI polling for events blocks the agent mid-turn.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        // SQLite ships with foreign keys OFF. Every ON DELETE CASCADE above is
        // decorative until this line runs.
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "busy_timeout", 5_000)?;

        let store = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let version: i32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;

        if version == 0 {
            conn.execute_batch(SCHEMA)?;
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        } else if version > SCHEMA_VERSION {
            bail!("database was written by a newer version (schema {version})");
        }
        // Future migrations: `if version < 2 { ... }`, bumping user_version.
        Ok(())
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub workspace: PathBuf,
    pub title: Option<String>,
    pub model: String,
    pub status: String,
    pub parent_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Store {
    pub fn create_session(&self, workspace: &Path, model: &str) -> Result<Session> {
        let id = uuid::Uuid::new_v4().to_string();
        let ts = now_ms();
        let conn = self.conn.lock().unwrap();

        conn.execute(
            "INSERT INTO sessions (id, workspace, model, status, created_at, updated_at)
             VALUES (?1, ?2, ?3, 'active', ?4, ?4)",
            params![id, workspace.display().to_string(), model, ts],
        )?;

        Ok(Session {
            id,
            workspace: workspace.to_path_buf(),
            title: None,
            model: model.to_string(),
            status: "active".into(),
            parent_id: None,
            created_at: ts,
            updated_at: ts,
        })
    }

    pub fn get_session(&self, id: &str) -> Result<Option<Session>> {
        let conn = self.conn.lock().unwrap();
        let row = conn
            .query_row(
                "SELECT id, workspace, title, model, status, parent_id, created_at, updated_at
                 FROM sessions WHERE id = ?1",
                params![id],
                |r| {
                    Ok(Session {
                        id: r.get(0)?,
                        workspace: PathBuf::from(r.get::<_, String>(1)?),
                        title: r.get(2)?,
                        model: r.get(3)?,
                        status: r.get(4)?,
                        parent_id: r.get(5)?,
                        created_at: r.get(6)?,
                        updated_at: r.get(7)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    pub fn recent_sessions(&self, limit: usize) -> Result<Vec<Session>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, workspace, title, model, status, parent_id, created_at, updated_at
             FROM sessions ORDER BY updated_at DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit as i64], |r| {
                Ok(Session {
                    id: r.get(0)?,
                    workspace: PathBuf::from(r.get::<_, String>(1)?),
                    title: r.get(2)?,
                    model: r.get(3)?,
                    status: r.get(4)?,
                    parent_id: r.get(5)?,
                    created_at: r.get(6)?,
                    updated_at: r.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn rename_session(&self, id: &str, title: &str) -> Result<()> {
        let title = title.trim();
        let conn = self.conn.lock().unwrap();
        let changed = conn.execute(
            "UPDATE sessions SET title = ?1, updated_at = ?2 WHERE id = ?3",
            params![
                if title.is_empty() { None } else { Some(title) },
                now_ms(),
                id
            ],
        )?;
        if changed == 0 {
            bail!("session `{id}` does not exist");
        }
        Ok(())
    }

    /// Remove a session and everything hanging off it. Patch pre-images go
    /// with it, so a deleted session can no longer be rolled back.
    pub fn delete_session(&self, id: &str) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        // Forks keep living when their parent goes away; parent_id has no ON
        // DELETE clause, so detach them first or the FK check refuses.
        tx.execute(
            "UPDATE sessions SET parent_id = NULL WHERE parent_id = ?1",
            params![id],
        )?;
        // tool_calls cascade from turns; patches and turns cascade from the
        // session row, but delete them explicitly so the intent is auditable.
        tx.execute("DELETE FROM patches WHERE session_id = ?1", params![id])?;
        tx.execute("DELETE FROM turns WHERE session_id = ?1", params![id])?;
        let changed = tx.execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
        tx.commit()?;
        if changed == 0 {
            bail!("session `{id}` does not exist");
        }
        Ok(())
    }

    /// Highest live seq of a session; the fork point for "fork at the tip".
    pub fn latest_seq(&self, session_id: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let seq: i64 = conn.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM turns WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )?;
        Ok(seq)
    }

    pub fn count_turns(&self, session_id: &str) -> Result<i64> {
        let conn = self.conn.lock().unwrap();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM turns WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )?;
        Ok(count)
    }

    /// `PRAGMA integrity_check` — "ok" on a healthy database.
    pub fn health(&self) -> Result<String> {
        let conn = self.conn.lock().unwrap();
        let verdict: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
        Ok(verdict)
    }

    /// Branch a session at a given turn. Cheap: copies rows, no filesystem work.
    /// Useful when a run went sideways and you want to retry from turn 12 with a
    /// different prompt instead of starting over.
    pub fn fork(&self, session_id: &str, at_seq: i64) -> Result<Session> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;

        let new_id = uuid::Uuid::new_v4().to_string();
        let ts = now_ms();

        tx.execute(
            "INSERT INTO sessions (id, workspace, title, model, status, parent_id,
                                   forked_at_seq, created_at, updated_at)
             SELECT ?1, workspace, title, model, 'active', id, ?2, ?3, ?3
             FROM sessions WHERE id = ?4",
            params![new_id, at_seq, ts, session_id],
        )?;

        tx.execute(
            "INSERT INTO turns (session_id, seq, role, content, tokens, is_summary,
                                superseded_by, pinned, created_at)
             SELECT ?1, seq, role, content, tokens, is_summary, NULL, pinned, created_at
             FROM turns
             WHERE session_id = ?2 AND seq <= ?3 AND superseded_by IS NULL",
            params![new_id, session_id, at_seq],
        )?;

        tx.commit()?;
        drop(conn);

        self.get_session(&new_id)?
            .context("forked session vanished")
    }
}

// ---------------------------------------------------------------------------
// Turns
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    pub id: i64,
    pub seq: i64,
    pub role: String,
    /// Serialised content blocks, in whatever shape your provider layer uses.
    pub content: String,
    pub tokens: i64,
    pub is_summary: bool,
    pub pinned: bool,
}

impl Store {
    pub fn append_turn(
        &self,
        session_id: &str,
        role: &str,
        content: &str,
        tokens: i64,
        pinned: bool,
    ) -> Result<Turn> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;

        let seq: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq), -1) + 1 FROM turns WHERE session_id = ?1",
            params![session_id],
            |r| r.get(0),
        )?;

        let ts = now_ms();
        tx.execute(
            "INSERT INTO turns (session_id, seq, role, content, tokens, pinned, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![session_id, seq, role, content, tokens, pinned as i64, ts],
        )?;
        let id = tx.last_insert_rowid();

        tx.execute(
            "UPDATE sessions SET updated_at = ?1 WHERE id = ?2",
            params![ts, session_id],
        )?;
        tx.commit()?;

        Ok(Turn {
            id,
            seq,
            role: role.to_string(),
            content: content.to_string(),
            tokens,
            is_summary: false,
            pinned,
        })
    }

    /// The turns that actually get sent to the model.
    pub fn live_turns(&self, session_id: &str) -> Result<Vec<Turn>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, seq, role, content, tokens, is_summary, pinned
             FROM turns
             WHERE session_id = ?1 AND superseded_by IS NULL
             ORDER BY seq",
        )?;
        let rows = stmt
            .query_map(params![session_id], |r| {
                Ok(Turn {
                    id: r.get(0)?,
                    seq: r.get(1)?,
                    role: r.get(2)?,
                    content: r.get(3)?,
                    tokens: r.get(4)?,
                    is_summary: r.get::<_, i64>(5)? != 0,
                    pinned: r.get::<_, i64>(6)? != 0,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Correct an estimated token count once the provider reports the real one.
    pub fn set_tokens(&self, turn_id: i64, tokens: i64) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE turns SET tokens = ?1 WHERE id = ?2",
            params![tokens, turn_id],
        )?;
        Ok(())
    }

    pub fn set_model(&self, session_id: &str, model: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE sessions SET model = ?1, updated_at = ?2 WHERE id = ?3",
            params![model, now_ms(), session_id],
        )?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Compaction
// ---------------------------------------------------------------------------

pub struct CompactionPlan {
    /// Turns to fold away, in order.
    pub victims: Vec<Turn>,
    pub freed_tokens: i64,
}

impl Store {
    /// Decide what to compact. Returns `None` when the session still fits.
    ///
    /// Two rules that are not optional:
    ///
    /// 1. Never cut between an assistant turn carrying a tool call and its
    ///    matching tool result. Every provider rejects a dangling tool_use, and
    ///    the error message is never about that. The cut point walks backwards
    ///    until it lands on a boundary where nothing is left hanging.
    ///
    /// 2. Pinned turns survive. That is the system prompt and the original
    ///    task. Summarise the goal away and the agent starts confidently
    ///    solving a different problem.
    pub fn plan_compaction(
        &self,
        session_id: &str,
        budget_tokens: i64,
        keep_recent: usize,
    ) -> Result<Option<CompactionPlan>> {
        let turns = self.live_turns(session_id)?;
        let total: i64 = turns.iter().map(|t| t.tokens).sum();

        if total <= budget_tokens {
            return Ok(None);
        }

        let cut_max = turns.len().saturating_sub(keep_recent);
        if cut_max == 0 {
            // Everything recent and still over budget. Nothing safe to do here;
            // the caller should surface this rather than silently truncating.
            return Ok(None);
        }

        let mut cut = cut_max;
        while cut > 0 && !is_safe_boundary(&turns, cut) {
            cut -= 1;
        }
        if cut == 0 {
            return Ok(None);
        }

        let victims: Vec<Turn> = turns[..cut].iter().filter(|t| !t.pinned).cloned().collect();
        if victims.is_empty() {
            return Ok(None);
        }

        let freed_tokens = victims.iter().map(|t| t.tokens).sum();
        Ok(Some(CompactionPlan {
            victims,
            freed_tokens,
        }))
    }

    /// Insert the summary and mark the victims superseded. The summary text
    /// comes from a separate, cheap model call the caller makes.
    pub fn commit_compaction(
        &self,
        session_id: &str,
        plan: &CompactionPlan,
        summary: &str,
        summary_tokens: i64,
    ) -> Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let ts = now_ms();

        let first_seq = plan.victims.first().map(|t| t.seq).unwrap_or(0);
        let archived_seq: i64 = tx.query_row(
            "SELECT COALESCE(MIN(seq), 0) - 1 FROM turns WHERE session_id = ?1",
            params![session_id],
            |row| row.get(0),
        )?;
        let first_id = plan.victims.first().map(|turn| turn.id).unwrap_or(0);

        tx.execute(
            "UPDATE turns SET seq = ?1 WHERE id = ?2 AND session_id = ?3",
            params![archived_seq, first_id, session_id],
        )?;

        tx.execute(
            "INSERT INTO turns (session_id, seq, role, content, tokens, is_summary, created_at)
             VALUES (?1, ?2, 'user', ?3, ?4, 1, ?5)",
            params![session_id, first_seq, summary, summary_tokens, ts],
        )?;
        let summary_id = tx.last_insert_rowid();

        for victim in &plan.victims {
            tx.execute(
                "UPDATE turns SET superseded_by = ?1 WHERE session_id = ?2 AND id = ?3",
                params![summary_id, session_id, victim.id],
            )?;
        }

        tx.commit()?;
        Ok(())
    }
}

/// A cut at index `i` is safe if no assistant turn before it has a tool call
/// whose result lands at or after it.
fn is_safe_boundary(turns: &[Turn], i: usize) -> bool {
    if i == 0 || i >= turns.len() {
        return true;
    }
    // A tool result must never be the first live turn after a cut.
    turns[i].role != "tool"
}

// ---------------------------------------------------------------------------
// Patches / rollback
// ---------------------------------------------------------------------------

/// Files above this go to a sidecar blob instead of the database. SQLite copes
/// with large blobs, but every `SELECT *` then drags them through memory.
const INLINE_BLOB_LIMIT: usize = 512 * 1024;

impl Store {
    pub fn record_patch(
        &self,
        session_id: &str,
        turn_id: Option<i64>,
        path: &Path,
        before: Option<&str>,
        after: Option<&str>,
        diff: &str,
    ) -> Result<i64> {
        if path.is_absolute()
            || path
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            bail!("patch path must be workspace-relative: {}", path.display());
        }
        if before.map_or(false, |b| b.len() > INLINE_BLOB_LIMIT) {
            // Left as an exercise: write to store/blobs/<sha256> and put the
            // digest in the column instead. Worth doing before you point this
            // at a repository with vendored dependencies.
            tracing::warn!(path = %path.display(), "storing large pre-image inline");
        }

        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO patches (session_id, turn_id, path, before, after, diff, applied_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                session_id,
                turn_id,
                path.display().to_string(),
                before,
                after,
                diff,
                now_ms()
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Undo every patch in the session, newest first. Returns the paths touched.
    ///
    /// Newest-first matters: if the agent edited the same file three times, the
    /// oldest pre-image is the one you want, and you only reach it correctly by
    /// unwinding in reverse.
    pub fn rollback_session(&self, session_id: &str, workspace: &Path) -> Result<Vec<PathBuf>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, path, before FROM patches
             WHERE session_id = ?1 AND reverted_at IS NULL
             ORDER BY id DESC",
        )?;

        let rows: Vec<(i64, String, Option<String>)> = stmt
            .query_map(params![session_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);

        let mut touched = Vec::new();
        let ts = now_ms();

        for (id, rel, before) in rows {
            let abs = apply_patch::resolve_path(workspace, Path::new(&rel))?;
            match before {
                Some(content) => {
                    if let Some(parent) = abs.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&abs, content)?;
                }
                None => {
                    std::fs::remove_file(&abs).ok();
                }
            }
            conn.execute(
                "UPDATE patches SET reverted_at = ?1 WHERE id = ?2",
                params![ts, id],
            )?;
            touched.push(abs);
        }

        Ok(touched)
    }

    pub fn session_diff(&self, session_id: &str) -> Result<String> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT diff FROM patches
             WHERE session_id = ?1 AND reverted_at IS NULL
             ORDER BY id",
        )?;
        let diffs = stmt
            .query_map(params![session_id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut unique = Vec::new();
        for diff in diffs {
            if unique.last() != Some(&diff) {
                unique.push(diff);
            }
        }
        Ok(unique.join("\n"))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn store() -> Store {
        let dir = std::env::temp_dir().join(format!("gnomef-test-{}", uuid::Uuid::new_v4()));
        Store::open(&dir.join("state.db")).unwrap()
    }

    #[test]
    fn append_and_load() {
        let s = store();
        let sess = s
            .create_session(Path::new("/tmp/ws"), "local-model")
            .unwrap();
        s.append_turn(&sess.id, "system", "you are an agent", 8, true)
            .unwrap();
        s.append_turn(&sess.id, "user", "fix the build", 5, true)
            .unwrap();

        let turns = s.live_turns(&sess.id).unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].seq, 0);
        assert!(turns[0].pinned);
    }

    #[test]
    fn database_file_is_private() {
        let dir = std::env::temp_dir().join(format!("gnomef-private-db-{}", uuid::Uuid::new_v4()));
        let path = dir.join("state.db");
        let _store = Store::open(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn fork_copies_prefix() {
        let s = store();
        let sess = s.create_session(Path::new("/tmp/ws"), "m").unwrap();
        for i in 0..5 {
            s.append_turn(&sess.id, "user", &format!("turn {i}"), 1, false)
                .unwrap();
        }
        let forked = s.fork(&sess.id, 2).unwrap();
        assert_eq!(s.live_turns(&forked.id).unwrap().len(), 3);
        assert_eq!(s.live_turns(&sess.id).unwrap().len(), 5);
    }

    #[test]
    fn rename_delete_and_fork_at_tip() {
        let s = store();
        let sess = s.create_session(Path::new("/tmp/ws"), "m").unwrap();
        for i in 0..4 {
            s.append_turn(&sess.id, "user", &format!("turn {i}"), 1, false)
                .unwrap();
        }

        s.rename_session(&sess.id, "  proiectul meu  ").unwrap();
        assert_eq!(
            s.get_session(&sess.id).unwrap().unwrap().title.as_deref(),
            Some("proiectul meu")
        );

        let tip = s.latest_seq(&sess.id).unwrap();
        let fork = s.fork(&sess.id, tip).unwrap();
        assert_eq!(s.live_turns(&fork.id).unwrap().len(), 4);

        s.delete_session(&sess.id).unwrap();
        assert!(s.get_session(&sess.id).unwrap().is_none());
        assert!(s.delete_session(&sess.id).is_err());
        // The fork survives its parent.
        assert_eq!(s.live_turns(&fork.id).unwrap().len(), 4);
        assert_eq!(s.health().unwrap(), "ok");
    }

    #[test]
    fn no_compaction_under_budget() {
        let s = store();
        let sess = s.create_session(Path::new("/tmp/ws"), "m").unwrap();
        s.append_turn(&sess.id, "user", "hi", 10, false).unwrap();
        assert!(s.plan_compaction(&sess.id, 1000, 2).unwrap().is_none());
    }
}
