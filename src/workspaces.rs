//! Persistent workspace history for the coding agent.
//!
//! Remembers the last workspace and a short most-recent-first list in
//! `store/workspaces.json`, so a bare `gnomef-rs` launched from $HOME can
//! reopen the project you actually work in instead of treating the whole
//! home directory as a repository.

use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::{
    fs::OpenOptions,
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

const MAX_RECENT: usize = 10;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct WorkspaceData {
    last: Option<String>,
    recent: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct WorkspaceHistory {
    path: PathBuf,
    data: WorkspaceData,
}

impl WorkspaceHistory {
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let data = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default();
        let mut history = Self { path, data };
        history.prune();
        history
    }

    /// The most recently used workspace, if it still exists on disk.
    pub fn last(&self) -> Option<PathBuf> {
        self.data
            .last
            .as_deref()
            .map(PathBuf::from)
            .filter(|path| path.is_dir())
    }

    /// Existing recent workspaces, newest first.
    pub fn recent(&self) -> Vec<PathBuf> {
        self.data
            .recent
            .iter()
            .map(PathBuf::from)
            .filter(|path| path.is_dir())
            .collect()
    }

    /// Record a workspace as most recent and persist. Errors are reported but
    /// never fatal: losing history must not break the agent.
    pub fn record(&mut self, workspace: &Path) {
        let entry = workspace.display().to_string();
        self.data.recent.retain(|item| item != &entry);
        self.data.recent.insert(0, entry.clone());
        self.data.recent.truncate(MAX_RECENT);
        self.data.last = Some(entry);
        if let Err(error) = self.save() {
            tracing::warn!("cannot persist workspace history: {error}");
        }
    }

    fn prune(&mut self) {
        self.data.recent.retain(|item| Path::new(item).is_dir());
        if self
            .data
            .last
            .as_deref()
            .is_some_and(|item| !Path::new(item).is_dir())
        {
            self.data.last = None;
        }
    }

    fn save(&self) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let raw = serde_json::to_vec_pretty(&self.data)?;
        let temporary = self.path.with_extension("json.tmp");
        let result = (|| -> anyhow::Result<()> {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(&raw)?;
            file.sync_all()?;
            std::fs::rename(&temporary, &self.path)
                .with_context(|| format!("failed to replace {}", self.path.display()))?;
            std::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o600))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result
    }
}

/// Decide the startup workspace.
///
/// Priority: explicit CLI argument, then — when launched from $HOME with no
/// argument — the remembered last workspace, then the launch directory itself.
/// Returns the chosen path plus an optional user-facing note explaining a
/// non-obvious choice.
pub fn resolve_startup_workspace(
    cli_workspace: Option<PathBuf>,
    launch_dir: &Path,
    history: &WorkspaceHistory,
) -> (PathBuf, Option<String>) {
    if let Some(explicit) = cli_workspace {
        return (explicit, None);
    }

    let home = std::env::var_os("HOME").map(PathBuf::from);
    let launched_from_home = home.as_deref().is_some_and(|home| home == launch_dir);
    if launched_from_home {
        if let Some(last) = history.last() {
            if last != launch_dir {
                let note = format!(
                    "started from your home directory — reopened the last workspace {} \
                     (use `gnomef-rs PATH` or /workspace to pick another)",
                    last.display()
                );
                return (last, Some(note));
            }
        }
        return (
            launch_dir.to_path_buf(),
            Some(
                "working directly in your home directory; pass a project path or use \
                 /workspace to switch to a repository"
                    .to_string(),
            ),
        );
    }

    (launch_dir.to_path_buf(), None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gnomef-ws-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn records_and_reloads_most_recent_first() {
        let dir = temp_dir("hist");
        let a = temp_dir("a");
        let b = temp_dir("b");
        let file = dir.join("workspaces.json");

        let mut history = WorkspaceHistory::load(&file);
        history.record(&a);
        history.record(&b);
        history.record(&a);

        let reloaded = WorkspaceHistory::load(&file);
        assert_eq!(reloaded.last().unwrap(), a);
        assert_eq!(reloaded.recent(), vec![a.clone(), b.clone()]);
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );

        for path in [dir, a, b] {
            std::fs::remove_dir_all(path).ok();
        }
    }

    #[test]
    fn missing_directories_are_pruned() {
        let dir = temp_dir("prune");
        let vanishing = temp_dir("gone");
        let file = dir.join("workspaces.json");

        let mut history = WorkspaceHistory::load(&file);
        history.record(&vanishing);
        std::fs::remove_dir_all(&vanishing).unwrap();

        let reloaded = WorkspaceHistory::load(&file);
        assert!(reloaded.last().is_none());
        assert!(reloaded.recent().is_empty());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn cli_argument_always_wins() {
        let dir = temp_dir("cli");
        let history = WorkspaceHistory::load(dir.join("workspaces.json"));
        let (chosen, note) = resolve_startup_workspace(
            Some(PathBuf::from("/explicit/path")),
            Path::new("/anywhere"),
            &history,
        );
        assert_eq!(chosen, PathBuf::from("/explicit/path"));
        assert!(note.is_none());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn home_launch_reopens_last_workspace() {
        let dir = temp_dir("home");
        let project = temp_dir("proj");
        let file = dir.join("workspaces.json");
        let mut history = WorkspaceHistory::load(&file);
        history.record(&project);

        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap();
        let (chosen, note) = resolve_startup_workspace(None, &home, &history);
        assert_eq!(chosen, project);
        assert!(note.unwrap().contains("last workspace"));

        std::fs::remove_dir_all(dir).ok();
        std::fs::remove_dir_all(project).ok();
    }
}
