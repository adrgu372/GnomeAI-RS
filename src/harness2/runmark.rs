//! Trial identity: was this rollout finished, and by this configuration?
//!
//! Completion is recorded explicitly by the code that observed the run return;
//! presence of output is not enough. The marker also fingerprints the harness
//! that produced the rollout, so a rollout from a different configuration is
//! never reused.

use std::path::Path;

use serde::{Deserialize, Serialize};

use super::{remove_tree_all, walk_files};

pub const MARK: &str = ".adapt_complete";
pub const VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarkPayload {
    pub version: u32,
    pub run_id: String,
    pub at: f64,
    pub harness: String,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

fn ignored(name: &str) -> bool {
    name == MARK || name == ".DS_Store" || name.ends_with(".pyc")
}

/// A stable digest of the harness tree that produced a rollout. `None` means a
/// base rollout on the inherited harness and digests to the shared value
/// "base".
pub fn fingerprint(harness_dir: Option<&Path>) -> String {
    use sha2::{Digest, Sha256};
    let Some(root) = harness_dir else {
        return "base".into();
    };
    if !root.is_dir() {
        return "absent".into();
    }
    let mut h = Sha256::new();
    for rel in walk_files(root) {
        let name = rel
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if ignored(&name) {
            continue;
        }
        let rel_str = rel.to_string_lossy();
        h.update(rel_str.as_bytes());
        h.update([0]);
        match std::fs::read(root.join(&rel)) {
            Ok(bytes) => h.update(&bytes),
            Err(_) => h.update(b"<unreadable>"),
        }
        h.update([0]);
    }
    hex16(&h.finalize())
}

fn hex16(digest: &[u8]) -> String {
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// Record that `run_id` finished. Call only after the runner returned normally.
pub fn mark_complete(
    where_dir: &Path,
    run_id: &str,
    harness_dir: Option<&Path>,
    extra: Option<serde_json::Map<String, serde_json::Value>>,
) -> std::io::Result<()> {
    std::fs::create_dir_all(where_dir)?;
    let payload = MarkPayload {
        version: VERSION,
        run_id: run_id.to_string(),
        at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or_default(),
        harness: fingerprint(harness_dir),
        extra: extra.unwrap_or_default(),
    };
    let tmp = where_dir.join(format!("{MARK}.tmp"));
    std::fs::write(&tmp, serde_json::to_vec_pretty(&payload)?)?;
    std::fs::rename(tmp, where_dir.join(MARK))
}

pub fn read_mark(where_dir: &Path) -> Option<MarkPayload> {
    let bytes = std::fs::read(where_dir.join(MARK)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Whether the artifacts at `where_dir` may be reused: a current marker naming
/// the same harness, unless `match_harness` is off.
pub fn is_complete(where_dir: &Path, harness_dir: Option<&Path>, match_harness: bool) -> bool {
    let Some(m) = read_mark(where_dir) else {
        return false;
    };
    if m.version != VERSION {
        return false;
    }
    if !match_harness {
        return true;
    }
    m.harness == fingerprint(harness_dir)
}

/// A sentence for a log line explaining why `where_dir` is or is not reusable.
pub fn why_not(where_dir: &Path, harness_dir: Option<&Path>) -> String {
    let Some(m) = read_mark(where_dir) else {
        return format!("no {MARK} -- never finished, or finished before completion was recorded");
    };
    if m.version != VERSION {
        return format!("marker version {} != {VERSION}", m.version);
    }
    let want = fingerprint(harness_dir);
    if want != m.harness {
        return format!(
            "harness fingerprint {} != {want} -- produced by a different configuration",
            m.harness
        );
    }
    "reusable".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_marker_round_trip() {
        let tmp = std::env::temp_dir().join(format!("h2-mark-{}", std::process::id()));
        remove_tree_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        assert!(!is_complete(&tmp, None, true));
        mark_complete(&tmp, "run-1", None, None).unwrap();
        assert!(is_complete(&tmp, None, true));
        // A different harness fingerprint invalidates reuse.
        assert!(!is_complete(
            &tmp,
            Some(std::path::Path::new("/nonexistent")),
            true
        ));
        assert_eq!(
            why_not(&tmp, Some(std::path::Path::new("/nonexistent"))).len() > 0,
            true
        );
        remove_tree_all(&tmp);
    }

    #[test]
    fn base_has_a_shared_fingerprint() {
        assert_eq!(fingerprint(None), "base");
        assert_eq!(
            fingerprint(Some(std::path::Path::new("/definitely/not/there"))),
            "absent"
        );
    }
}
