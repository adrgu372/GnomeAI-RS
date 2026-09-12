//! Durable transfer checkpoints. Source stays frozen until destination persists;
//! destination stays staged until source has relinquished execution.
use crate::store::{Session, Store, Turn};
use anyhow::{Context, Result, bail};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

#[derive(Serialize, Deserialize)]
pub struct PortableTurn {
    pub turn: Turn,
    pub superseded_seq: Option<i64>,
    pub created_at: i64,
}
#[derive(Serialize, Deserialize)]
pub struct SessionSnapshot {
    #[serde(default)]
    pub workspace_id: String,
    pub version: u32,
    pub transfer_id: String,
    pub session: Session,
    pub provider_id: String,
    pub reasoning_effort: String,
    pub turns: Vec<PortableTurn>,
    #[serde(default)]
    pub outputs: BTreeMap<String, String>,
}
impl Store {
    pub fn device_schema(&self) -> Result<()> {
        // Acknowledged ownership changes must survive a database restart.
        self.conn
            .lock()
            .unwrap()
            .pragma_update(None, "synchronous", "FULL")?;
        self.conn.lock().unwrap().execute_batch("CREATE TABLE IF NOT EXISTS device_transfers (
            transfer_id TEXT PRIMARY KEY, session_id TEXT NOT NULL, peer TEXT NOT NULL,
            direction TEXT NOT NULL, phase TEXT NOT NULL, snapshot TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS device_commands (request_id TEXT PRIMARY KEY, response TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS device_session_settings (session_id TEXT PRIMARY KEY, provider TEXT NOT NULL, reasoning TEXT NOT NULL);")?;
        self.conn.lock().unwrap().execute_batch("CREATE TABLE IF NOT EXISTS device_workspaces (path TEXT PRIMARY KEY, id TEXT NOT NULL UNIQUE);")?;
        self.conn.lock().unwrap().execute_batch("CREATE TABLE IF NOT EXISTS device_workspace_locks (path TEXT PRIMARY KEY, token TEXT NOT NULL); DELETE FROM device_workspace_locks;")?;
        Ok(())
    }
    pub fn workspace_identity(&self,path:&Path)->Result<String> {
        let conn=self.conn.lock().unwrap();
        conn.execute("INSERT OR IGNORE INTO device_workspaces(path,id) VALUES (?1,?2)",params![path.display().to_string(),uuid::Uuid::new_v4().to_string()])?;
        Ok(conn.query_row("SELECT id FROM device_workspaces WHERE path=?1",[path.display().to_string()],|r|r.get(0))?)
    }
    pub fn remember_execution(&self, id: &str, provider: &str, reasoning: &str) -> Result<()> {
        self.conn.lock().unwrap().execute("INSERT INTO device_session_settings VALUES (?1,?2,?3) ON CONFLICT(session_id) DO UPDATE SET provider=excluded.provider,reasoning=excluded.reasoning",params![id,provider,reasoning])?;
        Ok(())
    }
    pub fn session_execution(&self, id: &str) -> Result<Option<(String, String)>> {
        Ok(self
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT provider,reasoning FROM device_session_settings WHERE session_id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }
    pub fn device_response(&self, request_id: &str) -> Result<Option<serde_json::Value>> {
        let text: Option<String> = self
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT response FROM device_commands WHERE request_id=?1",
                [request_id],
                |r| r.get(0),
            )
            .optional()?;
        text.map(|t| serde_json::from_str(&t).map_err(Into::into))
            .transpose()
    }
    pub fn save_device_response(
        &self,
        request_id: &str,
        response: &serde_json::Value,
    ) -> Result<()> {
        self.conn.lock().unwrap().execute("INSERT INTO device_commands VALUES (?1,?2) ON CONFLICT(request_id) DO UPDATE SET response=excluded.response",
            params![request_id, response.to_string()])?;
        Ok(())
    }
    pub fn assert_session_writable(&self, id: &str) -> Result<()> {
        let session = self.get_session(id)?.context("Session does not exist")?;
        let locked:i64=self.conn.lock().unwrap().query_row("SELECT COUNT(*) FROM device_workspace_locks WHERE path=?1",[session.workspace.display().to_string()],|r|r.get(0))?;
        if locked!=0 {bail!("Workspace sync is applying a file. Retry in a moment.");}
        if session.status != "active" {
            bail!(
                "Session execution is {}. Finish the transfer or open it on its owning device.",
                session.status
            );
        }
        Ok(())
    }
    pub fn lock_workspace(&self,path:&Path,token:&str,unlock:bool)->Result<()> {
        let conn=self.conn.lock().unwrap();
        if unlock {conn.execute("DELETE FROM device_workspace_locks WHERE path=?1 AND token=?2",params![path.display().to_string(),token])?;}
        else {conn.execute("INSERT INTO device_workspace_locks(path,token) VALUES (?1,?2)",params![path.display().to_string(),token])?;}
        Ok(())
    }
    pub fn prepare_handoff(
        &self,
        id: &str,
        transfer: &str,
        peer: &str,
        provider: &str,
        reasoning: &str,
        output_root: &Path,
    ) -> Result<serde_json::Value> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let old:Option<String> = tx.query_row("SELECT snapshot FROM device_transfers WHERE transfer_id=?1 AND session_id=?2 AND peer=?3 AND direction='out'",
            params![transfer,id,peer],|r|r.get(0)).optional()?;
        if let Some(old) = old {
            return Ok(serde_json::from_str(&old)?);
        }
        let session = tx.query_row(
            "SELECT id,workspace,title,model,status,parent_id,created_at,updated_at FROM sessions WHERE id=?1",
            [id], |r| Ok(Session { id:r.get(0)?, workspace:std::path::PathBuf::from(r.get::<_,String>(1)?),
                title:r.get(2)?, model:r.get(3)?, status:r.get(4)?, parent_id:r.get(5)?, created_at:r.get(6)?, updated_at:r.get(7)? })
        ).optional()?.context("Session missing")?;
        if session.status != "active" {
            bail!("Session is already transferring or owned by another device");
        }
        let locked:i64=tx.query_row("SELECT COUNT(*) FROM device_workspace_locks WHERE path=?1",[session.workspace.display().to_string()],|r|r.get(0))?;
        if locked!=0 {bail!("Workspace sync is applying a file; retry the transfer");}
        let turns = tx.prepare("SELECT t.id,t.seq,t.role,t.content,t.tokens,t.is_summary,t.pinned,s.seq,t.created_at
            FROM turns t LEFT JOIN turns s ON s.id=t.superseded_by WHERE t.session_id=?1 ORDER BY t.seq")?
            .query_map([id], |r| Ok(PortableTurn { turn:Turn { id:r.get(0)?,seq:r.get(1)?,role:r.get(2)?,content:r.get(3)?,tokens:r.get(4)?,is_summary:r.get(5)?,pinned:r.get(6)? }, superseded_seq:r.get(7)?, created_at:r.get(8)? }))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        validate_turns(&turns)?;
        let mut outputs = BTreeMap::new();
        let matcher = regex::Regex::new(r"full output handle: ([0-9a-fA-F]{32});")?;
        let mut bytes = 0usize;
        for turn in &turns {
            for found in matcher.captures_iter(&turn.turn.content) {
                let handle = found[1].to_string();
                if outputs.contains_key(&handle) {
                    continue;
                }
                let path = output_root.join(format!("{handle}.log"));
                bytes=bytes.checked_add(path.metadata().with_context(||format!("Tool output {handle} is missing; retain the session on its current device"))?.len() as usize).context("Output size overflow")?;
                if bytes > 16 * 1024 * 1024 {
                    bail!("Tool outputs exceed the 16 MiB transfer limit");
                }
                outputs.insert(handle, std::fs::read_to_string(path)?);
            }
        }
        let snapshot = SessionSnapshot {
            workspace_id: {
                tx.execute("INSERT OR IGNORE INTO device_workspaces(path,id) VALUES (?1,?2)",params![session.workspace.display().to_string(),uuid::Uuid::new_v4().to_string()])?;
                tx.query_row("SELECT id FROM device_workspaces WHERE path=?1",[session.workspace.display().to_string()],|r|r.get(0))?
            },
            version: 1,
            transfer_id: transfer.into(),
            session,
            provider_id: provider.into(),
            reasoning_effort: reasoning.into(),
            turns,
            outputs,
        };
        let value = serde_json::to_value(snapshot)?;
        let text = value.to_string();
        if text.len() > 16 * 1024 * 1024 {
            bail!("Session exceeds the 16 MiB transfer limit");
        }
        let changed = tx.execute(
            "UPDATE sessions SET status='transferring' WHERE id=?1 AND status='active'",
            [id],
        )?;
        if changed != 1 {
            bail!("Session is already transferring");
        }
        tx.execute(
            "INSERT INTO device_transfers VALUES (?1,?2,?3,'out','prepared',?4)",
            params![transfer, id, peer, text],
        )?;
        tx.commit()?;
        Ok(value)
    }
    pub fn stage_handoff(
        &self,
        snapshot: &SessionSnapshot,
        peer: &str,
        workspace: &Path,
        output_root: &Path,
    ) -> Result<()> {
        if snapshot.version != 1 || snapshot.turns.len() > 100_000 {
            bail!("Unsupported session snapshot");
        }
        validate_turns(&snapshot.turns)?;
        if serde_json::to_vec(snapshot)?.len() > 16 * 1024 * 1024 {
            bail!("Snapshot exceeds 16 MiB");
        }
        uuid::Uuid::parse_str(&snapshot.session.id)?;
        uuid::Uuid::parse_str(&snapshot.transfer_id)?;
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let existing:Option<String> = tx.query_row("SELECT phase FROM device_transfers WHERE transfer_id=?1 AND peer=?2 AND direction='in'",
            params![snapshot.transfer_id,peer],|r|r.get(0)).optional()?;
        if existing.is_some() {
            return Ok(());
        }
        let status: Option<String> = tx
            .query_row(
                "SELECT status FROM sessions WHERE id=?1",
                [&snapshot.session.id],
                |r| r.get(0),
            )
            .optional()?;
        if status.as_deref().is_some_and(|s| s != "remote") {
            bail!("A local session with this identity already owns execution");
        }
        // Preserve the session row for fork references. Incoming content replaces
        // only a previously transferred remote cache, never an active session.
        tx.execute(
            "DELETE FROM patches WHERE session_id=?1",
            [&snapshot.session.id],
        )?;
        tx.execute(
            "DELETE FROM turns WHERE session_id=?1",
            [&snapshot.session.id],
        )?;
        tx.execute("INSERT INTO sessions(id,workspace,title,model,status,created_at,updated_at) VALUES (?1,?2,?3,?4,'staged',?5,?6)
            ON CONFLICT(id) DO UPDATE SET workspace=excluded.workspace,title=excluded.title,model=excluded.model,status='staged',updated_at=excluded.updated_at",
            params![snapshot.session.id,workspace.display().to_string(),snapshot.session.title,snapshot.session.model,snapshot.session.created_at,snapshot.session.updated_at])?;
        std::fs::create_dir_all(output_root)?;
        for (handle, content) in &snapshot.outputs {
            if handle.len() != 32 || !handle.bytes().all(|c| c.is_ascii_hexdigit()) {
                bail!("Invalid output handle");
            }
            let path = output_root.join(format!("{handle}.log"));
            if path.exists() {
                if std::fs::read_to_string(&path)? != *content {
                    bail!("Output handle collision");
                }
            } else {
                let temporary = output_root.join(format!("{handle}.{}.tmp", uuid::Uuid::new_v4()));
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&temporary)?;
                file.write_all(content.as_bytes())?;
                file.sync_all()?;
                drop(file);
                std::fs::rename(temporary, path)?;
                std::fs::File::open(output_root)?.sync_all()?;
            }
        }
        let mut ids = HashMap::new();
        for portable in &snapshot.turns {
            let t = &portable.turn;
            if !matches!(t.role.as_str(), "system" | "user" | "assistant" | "tool") {
                bail!("Invalid turn role");
            }
            tx.execute("INSERT INTO turns(session_id,seq,role,content,tokens,is_summary,pinned,created_at) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
                params![snapshot.session.id,t.seq,t.role,t.content,t.tokens,t.is_summary,t.pinned,portable.created_at])?;
            ids.insert(t.seq, tx.last_insert_rowid());
        }
        for portable in &snapshot.turns {
            if let Some(seq) = portable.superseded_seq {
                let replacement = ids.get(&seq).context("Invalid compaction reference")?;
                tx.execute(
                    "UPDATE turns SET superseded_by=?1 WHERE id=?2",
                    params![replacement, ids[&portable.turn.seq]],
                )?;
            }
        }
        tx.execute("INSERT INTO device_session_settings VALUES (?1,?2,?3) ON CONFLICT(session_id) DO UPDATE SET provider=excluded.provider,reasoning=excluded.reasoning",
            params![snapshot.session.id,snapshot.provider_id,snapshot.reasoning_effort])?;
        if !snapshot.workspace_id.is_empty() {
            uuid::Uuid::parse_str(&snapshot.workspace_id)?;
            tx.execute("INSERT INTO device_workspaces(path,id) VALUES (?1,?2) ON CONFLICT(path) DO UPDATE SET id=excluded.id",params![workspace.display().to_string(),snapshot.workspace_id])?;
        }
        tx.execute(
            "INSERT INTO device_transfers VALUES (?1,?2,?3,'in','staged',?4)",
            params![
                snapshot.transfer_id,
                snapshot.session.id,
                peer,
                serde_json::to_string(snapshot)?
            ],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn finish_handoff(&self, transfer: &str, peer: &str, incoming: bool) -> Result<String> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let direction = if incoming { "in" } else { "out" };
        let (id,phase):(String,String)=tx.query_row("SELECT session_id,phase FROM device_transfers WHERE transfer_id=?1 AND peer=?2 AND direction=?3",
            params![transfer,peer,direction],|r|Ok((r.get(0)?,r.get(1)?)))?;
        if phase == "complete" {
            return Ok(id);
        }
        let (before, after) = if incoming {
            ("staged", "active")
        } else {
            ("transferring", "remote")
        };
        if tx.execute(
            "UPDATE sessions SET status=?1 WHERE id=?2 AND status=?3",
            params![after, id, before],
        )? != 1
        {
            bail!("Invalid transfer state");
        }
        tx.execute(
            "UPDATE device_transfers SET phase='complete' WHERE transfer_id=?1",
            [transfer],
        )?;
        tx.commit()?;
        Ok(id)
    }
}

fn validate_turns(turns: &[PortableTurn]) -> Result<()> {
    let mut pending = HashSet::new();
    for item in turns.iter().filter(|t| t.superseded_seq.is_none()) {
        if matches!(item.turn.role.as_str(), "system" | "user") {
            continue;
        }
        let message: crate::provider::Message = serde_json::from_str(&item.turn.content)?;
        match message {
            crate::provider::Message::Assistant { tool_calls, .. } => {
                for call in tool_calls {
                    pending.insert(call.id);
                }
            }
            crate::provider::Message::Tool { call_id, .. } => {
                if !pending.remove(&call_id) {
                    bail!("Session has an unmatched tool result");
                }
            }
            _ => {}
        }
    }
    if !pending.is_empty() {
        bail!("Finish the pending tool calls before moving this session");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture {
        root: std::path::PathBuf,
        store: Store,
    }
    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("gnomeai-handoff-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(root.join("outputs")).unwrap();
            let store = Store::open(&root.join("agent.db")).unwrap();
            store.device_schema().unwrap();
            Self { root, store }
        }
        fn session(&self) -> String {
            let id = self
                .store
                .create_session(&self.root, "test-model")
                .unwrap()
                .id;
            self.store
                .append_turn(&id, "user", "Continue this task", 5, true)
                .unwrap();
            id
        }
        fn prepare(&self, id: &str, transfer: &str, peer: &str) -> SessionSnapshot {
            serde_json::from_value(
                self.store
                    .prepare_handoff(
                        id,
                        transfer,
                        peer,
                        "custom",
                        "high",
                        &self.root.join("outputs"),
                    )
                    .unwrap(),
            )
            .unwrap()
        }
        fn stage(&self, snapshot: &SessionSnapshot, peer: &str) -> Result<()> {
            self.store
                .stage_handoff(snapshot, peer, &self.root, &self.root.join("outputs"))
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn workspace_lock_blocks_session_start_and_handoff_until_owner_unlocks() {
        let fixture=Fixture::new();let id=fixture.session();
        fixture.store.lock_workspace(&fixture.root,"sync-owner",false).unwrap();
        assert!(fixture.store.assert_session_writable(&id).is_err());
        assert!(fixture.store.prepare_handoff(&id,&uuid::Uuid::new_v4().to_string(),"peer","custom","high",&fixture.root.join("outputs")).is_err());
        fixture.store.lock_workspace(&fixture.root,"other-owner",true).unwrap();
        assert!(fixture.store.assert_session_writable(&id).is_err());
        fixture.store.lock_workspace(&fixture.root,"sync-owner",true).unwrap();
        fixture.store.assert_session_writable(&id).unwrap();
    }

    #[test]
    fn workspace_identity_survives_handoff_to_a_different_path() {
        let source=Fixture::new();let target=Fixture::new();let id=source.session();
        let expected=source.store.workspace_identity(&source.root).unwrap();
        let snapshot=source.prepare(&id,&uuid::Uuid::new_v4().to_string(),"peer");
        assert_eq!(snapshot.workspace_id,expected);
        target.stage(&snapshot,"peer").unwrap();
        assert_eq!(target.store.workspace_identity(&target.root).unwrap(),expected);
    }

    #[test]
    fn transfer_never_gives_both_devices_execution_and_retries_are_idempotent() {
        let source = Fixture::new();
        let target = Fixture::new();
        let id = source.session();
        let transfer = uuid::Uuid::new_v4().to_string();
        let peer = "paired-device";
        let snapshot = source.prepare(&id, &transfer, peer);
        assert!(source.store.assert_session_writable(&id).is_err());
        target.stage(&snapshot, peer).unwrap();
        assert!(target.store.assert_session_writable(&id).is_err());
        source.store.finish_handoff(&transfer, peer, false).unwrap();
        assert!(
            source
                .store
                .append_turn(&id, "user", "must not execute here", 1, false)
                .is_err()
        );
        target.store.finish_handoff(&transfer, peer, true).unwrap();
        target.store.assert_session_writable(&id).unwrap();
        target.stage(&snapshot, peer).unwrap();
        source.store.finish_handoff(&transfer, peer, false).unwrap();
        target.store.finish_handoff(&transfer, peer, true).unwrap();
        assert_eq!(target.store.count_turns(&id).unwrap(), 1);
        assert_eq!(
            target.store.session_execution(&id).unwrap(),
            Some(("custom".into(), "high".into()))
        );
    }
    #[test]
    fn restart_preserves_a_staged_transfer_and_round_trip_preserves_new_turns() {
        let source = Fixture::new();
        let target = Fixture::new();
        let id = source.session();
        let peer = "peer";
        let transfer = uuid::Uuid::new_v4().to_string();
        let snapshot = source.prepare(&id, &transfer, peer);
        target.stage(&snapshot, peer).unwrap();
        let reopened = Store::open(&target.root.join("agent.db")).unwrap();
        reopened.device_schema().unwrap();
        assert!(reopened.assert_session_writable(&id).is_err());
        source.store.finish_handoff(&transfer, peer, false).unwrap();
        reopened.finish_handoff(&transfer, peer, true).unwrap();
        reopened
            .append_turn(&id, "user", "New work on the phone", 6, false)
            .unwrap();
        let back = uuid::Uuid::new_v4().to_string();
        let returning = target.prepare(&id, &back, peer);
        source.stage(&returning, peer).unwrap();
        target.store.finish_handoff(&back, peer, false).unwrap();
        source.store.finish_handoff(&back, peer, true).unwrap();
        assert_eq!(
            source
                .store
                .live_turns(&id)
                .unwrap()
                .last()
                .unwrap()
                .content,
            "New work on the phone"
        );
    }
    #[test]
    fn missing_tool_result_blocks_handoff_without_freezing_source() {
        let source = Fixture::new();
        let id = source.session();
        source.store.append_turn(&id,"assistant",r#"{"role":"assistant","content":"","tool_calls":[{"id":"call-1","name":"web_search","arguments":"{}"}]}"#,12,false).unwrap();
        assert!(
            source
                .store
                .prepare_handoff(
                    &id,
                    &uuid::Uuid::new_v4().to_string(),
                    "peer",
                    "custom",
                    "default",
                    &source.root.join("outputs")
                )
                .is_err()
        );
        source.store.assert_session_writable(&id).unwrap();
    }
    #[test]
    fn bad_snapshot_does_not_replace_an_active_session() {
        let source = Fixture::new();
        let target = Fixture::new();
        let id = source.session();
        let transfer = uuid::Uuid::new_v4().to_string();
        let mut snapshot = source.prepare(&id, &transfer, "peer");
        let target_id = target.session();
        snapshot.session.id = target_id.clone();
        assert!(target.stage(&snapshot, "peer").is_err());
        target.store.assert_session_writable(&target_id).unwrap();
        assert_eq!(target.store.count_turns(&target_id).unwrap(), 1);
    }
}
