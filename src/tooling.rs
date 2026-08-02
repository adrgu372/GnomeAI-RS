//! Shared primitives for native Rust tools.
//!
//! A tool owns its provider-facing schema, execution policy and implementation.
//! Keeping those together prevents the schema/dispatcher drift that previously
//! occurred when each interface maintained its own description of a tool.

use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio_util::sync::CancellationToken;

use crate::provider::ToolSpec;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolEffect {
    WorkspaceRead,
    WorkspaceWrite,
    NetworkRead,
    UserProcess,
    PrivilegeEscalation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolConcurrency {
    Parallel,
    Exclusive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalRequirement {
    None,
    /// Obeys the selected normal/full-access policy.
    Standard,
    /// Root or equivalent privilege escalation. Full-access never implies root.
    Privileged,
}

#[derive(Debug, Clone)]
pub struct ToolDefinition {
    pub spec: ToolSpec,
    pub effects: Vec<ToolEffect>,
    pub concurrency: ToolConcurrency,
    pub approval: ApprovalRequirement,
}

impl ToolDefinition {
    pub fn workspace_read(spec: ToolSpec) -> Self {
        Self {
            spec,
            effects: vec![ToolEffect::WorkspaceRead],
            concurrency: ToolConcurrency::Parallel,
            approval: ApprovalRequirement::None,
        }
    }

    pub fn network_read(spec: ToolSpec) -> Self {
        Self {
            spec,
            effects: vec![ToolEffect::NetworkRead],
            concurrency: ToolConcurrency::Parallel,
            approval: ApprovalRequirement::None,
        }
    }

    pub fn workspace_write(spec: ToolSpec) -> Self {
        Self {
            spec,
            effects: vec![ToolEffect::WorkspaceWrite],
            concurrency: ToolConcurrency::Exclusive,
            approval: ApprovalRequirement::Standard,
        }
    }

    pub fn user_process(spec: ToolSpec) -> Self {
        Self {
            spec,
            effects: vec![ToolEffect::UserProcess, ToolEffect::WorkspaceWrite],
            concurrency: ToolConcurrency::Exclusive,
            approval: ApprovalRequirement::Standard,
        }
    }

    pub fn privileged(spec: ToolSpec) -> Self {
        Self {
            spec,
            effects: vec![ToolEffect::PrivilegeEscalation],
            concurrency: ToolConcurrency::Exclusive,
            approval: ApprovalRequirement::Privileged,
        }
    }
}

pub struct FilePatch {
    /// Workspace-relative path.
    pub path: PathBuf,
    pub before: Option<String>,
    pub after: Option<String>,
    pub diff: String,
}

pub struct ToolOutcome {
    /// What the model sees after central output handling.
    pub content: String,
    pub ok: bool,
    /// Files this call wrote, if any. Non-empty triggers verification.
    pub touched: Vec<PathBuf>,
    /// Reversible file changes, normally produced by `apply_patch`.
    pub patches: Vec<FilePatch>,
}

#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDefinition;
    async fn call(&self, args: Value, cancel: &CancellationToken) -> Result<ToolOutcome>;
}

#[derive(Default)]
pub struct Registry {
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl Registry {
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.definition().spec.name;
        assert!(
            self.tools.insert(name.clone(), tool).is_none(),
            "duplicate native tool registration: {name}"
        );
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools
            .values()
            .map(|tool| tool.definition().spec)
            .collect()
    }

    pub fn find(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }
}

#[derive(Debug, Clone)]
pub struct ToolOutputStore {
    root: PathBuf,
    max_inline_bytes: usize,
}

impl ToolOutputStore {
    pub fn new(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(&root).with_context(|| {
            format!("failed to create tool output directory {}", root.display())
        })?;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).with_context(|| {
            format!("failed to protect tool output directory {}", root.display())
        })?;
        let store = Self {
            root,
            max_inline_bytes: 32 * 1024,
        };
        store.cleanup()?;
        Ok(store)
    }

    pub fn present(&self, content: &str) -> Result<String> {
        if content.len() <= self.max_inline_bytes {
            return Ok(content.to_string());
        }

        let handle = uuid::Uuid::new_v4().simple().to_string();
        let path = self.path_for(&handle)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("failed to create {}", path.display()))?;
        use std::io::Write;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;

        let head_budget = self.max_inline_bytes * 2 / 3;
        let tail_budget = self.max_inline_bytes - head_budget;
        let head = prefix_at_boundary(content, head_budget);
        let tail = suffix_at_boundary(content, tail_budget);
        Ok(format!(
            "{head}\n\n[... {} bytes stored outside the conversation ...]\n\n{tail}\n\n\
             [full output handle: {handle}; use read_tool_output to inspect it]",
            content.len().saturating_sub(head.len() + tail.len())
        ))
    }

    pub fn read(&self, handle: &str, offset: usize, limit: usize) -> Result<String> {
        let path = self.path_for(handle)?;
        let text = fs::read_to_string(&path)
            .with_context(|| format!("unknown or unreadable tool output handle `{handle}`"))?;
        let offset = offset.max(1);
        let limit = limit.clamp(1, 800);
        let lines: Vec<&str> = text.lines().collect();
        let start = (offset - 1).min(lines.len());
        let end = (start + limit).min(lines.len());
        let mut output = String::new();
        for (index, line) in lines[start..end].iter().enumerate() {
            output.push_str(&format!("{:>6}  {line}\n", offset + index));
        }
        if end < lines.len() {
            output.push_str(&format!(
                "\n[{} more lines; continue with offset={}]",
                lines.len() - end,
                end + 1
            ));
        }
        if output.is_empty() {
            output.push_str("(empty output)");
        }
        Ok(output)
    }

    fn path_for(&self, handle: &str) -> Result<PathBuf> {
        if handle.len() != 32 || !handle.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("invalid tool output handle");
        }
        let path = self.root.join(format!("{handle}.log"));
        if path.parent() != Some(self.root.as_path()) {
            bail!("invalid tool output handle");
        }
        Ok(path)
    }

    fn cleanup(&self) -> Result<()> {
        let cutoff = SystemTime::now()
            .checked_sub(Duration::from_secs(7 * 24 * 60 * 60))
            .unwrap_or(SystemTime::UNIX_EPOCH);
        for entry in fs::read_dir(&self.root)? {
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("log") {
                continue;
            }
            let old = entry
                .metadata()
                .and_then(|metadata| metadata.modified())
                .is_ok_and(|modified| modified < cutoff);
            if old {
                let _ = fs::remove_file(path);
            }
        }
        Ok(())
    }
}

fn prefix_at_boundary(text: &str, budget: usize) -> &str {
    let mut end = budget.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn suffix_at_boundary(text: &str, budget: usize) -> &str {
    let mut start = text.len().saturating_sub(budget);
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_handles_reject_paths() {
        let root = std::env::temp_dir().join(format!("gnomef-tooling-{}", uuid::Uuid::new_v4()));
        let store = ToolOutputStore::new(root.clone()).unwrap();
        assert!(store.read("../../etc/passwd", 1, 10).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn output_preview_preserves_unicode_boundaries() {
        let root = std::env::temp_dir().join(format!("gnomef-tooling-{}", uuid::Uuid::new_v4()));
        let mut store = ToolOutputStore::new(root.clone()).unwrap();
        store.max_inline_bytes = 64;
        let result = store.present(&"ă".repeat(100)).unwrap();
        assert!(result.contains("full output handle:"));
        let _ = fs::remove_dir_all(root);
    }
}
