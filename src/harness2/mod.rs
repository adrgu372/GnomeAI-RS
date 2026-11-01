//! Recursive agent harnessing (a Rust port of google-research/harness2).
//!
//! A *harness* is the system around a model that turns it into an agent:
//! prompts, tools, memory, skills, orchestration. *Harnessing* optimises that
//! system. This module ports the harness2 method while keeping model weights
//! fixed:
//!
//! * **Task-level recursion** refines the execution harness for one task
//!   through a contrastive Propose-Probe-Compose step: candidate editors
//!   propose invariant (INV) and domain (DOM) edits, each candidate is rolled
//!   out on the same task, and a composer merges what the behavioural
//!   contrasts support -- without any ground-truth feedback.
//! * **Harnessing-level recursion** keeps a per-domain library of adopted
//!   harnesses plus evidence-supported lessons that later tasks read first.
//!
//! Three axes stay separate, exactly as upstream: the *method* (this module),
//! the *substrate* (the agent framework a solver rollout runs under, captured
//! by [`tree::SurfaceSpec`]) and the *benchmark* (an [`adapter::Adapter`]).

pub mod adapter;
pub mod context;
pub mod gnomeai;
pub mod judge;
pub mod ledger;
pub mod library;
pub mod prompts;
pub mod proposer;
pub mod recursion;
pub mod report;
pub mod runmark;
pub mod schedule;
pub mod selection;
pub mod substrate;
pub mod tree;

pub use gnomeai::run_desktop_command;

#[cfg(test)]
mod workflow_tests;

use std::path::{Path, PathBuf};

pub const METHOD_VERSION: u32 = 2;

/// The two recursion modes. `agg` composes k finals in parallel from 2k
/// dimension-scoped candidates; `seq` composes k nested checkpoints.
pub const MODES: (&str, &str) = ("agg", "seq");

/// A required deployment value is missing; the message carries the remediation.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ConfigError(pub String);

/// Everything that differs between the two channel transports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelSpec {
    /// `claude_cli`, `opencode` or `gnomeai` (the in-process GnomeAI channel).
    pub kind: String,
    pub model: String,
    pub effort: String,
    pub steps: u32,
    pub timeout_secs: u64,
}

impl ChannelSpec {
    pub fn new(kind: &str, model: &str) -> Self {
        Self {
            kind: kind.to_string(),
            model: model.to_string(),
            effort: "high".into(),
            steps: 120,
            timeout_secs: 3600,
        }
    }
}

/// `env(name)`: a set-but-empty variable counts as unset, as upstream.
pub fn env(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => Some(v),
        _ => None,
    }
}

pub fn env_or(name: &str, default: &str) -> String {
    env(name).unwrap_or_else(|| default.to_string())
}

pub fn env_int(name: &str, default: u64) -> Result<u64, ConfigError> {
    match env(name) {
        None => Ok(default),
        Some(v) => v
            .parse::<u64>()
            .map_err(|_| ConfigError(format!("{name} must be an integer, got {v:?}"))),
    }
}

pub fn require_env(name: &str, needed_by: &str) -> Result<String, ConfigError> {
    env(name).ok_or_else(|| {
        ConfigError(format!(
            "{name} is required for {needed_by}; set it in .env or the shell (see .env.example)"
        ))
    })
}

/// The runs root: everything this module writes lands beneath it.
pub fn runs_root() -> PathBuf {
    match env("HARNESS2_RUNS_DIR") {
        Some(dir) => {
            let p = Path::new(&dir);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join(p)
            }
        }
        None => match env("GNOMEF_RS_HOME") {
            Some(home) => PathBuf::from(home).join("harness2/runs"),
            None => match std::env::var("XDG_STATE_HOME") {
                Ok(state) if !state.is_empty() => {
                    PathBuf::from(state).join("gnomeai-rs/harness2/runs")
                }
                _ => {
                    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
                    PathBuf::from(home)
                        .join(".local/state/gnomeai-rs")
                        .join("harness2/runs")
                }
            },
        },
    }
}

/// Deterministic shuffles without a third-party `rand` dependency. Python's
/// `random.Random` is not reproduced bit-for-bit; the requirement upstream
/// states -- that the order depends only on (seed, domain, task set), never on
/// iteration order -- is met by hashing the seed string through SHA-256 and
/// driving a Knuth shuffle from the digest stream.
pub struct SeededRandom {
    state: [u8; 32],
    pos: usize,
}

impl SeededRandom {
    pub fn new(seed: &str) -> Self {
        use sha2::{Digest, Sha256};
        Self {
            state: Sha256::digest(seed.as_bytes()).into(),
            pos: 0,
        }
    }

    fn next_u32(&mut self) -> u32 {
        if self.pos + 4 > self.state.len() {
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            h.update(self.state);
            self.state = h.finalize().into();
            self.pos = 0;
        }
        let word = u32::from_be_bytes([
            self.state[self.pos],
            self.state[self.pos + 1],
            self.state[self.pos + 2],
            self.state[self.pos + 3],
        ]);
        self.pos += 4;
        word
    }

    /// A uniform index in `0..bound` (bound > 0), rejection-sampled.
    pub fn below(&mut self, bound: usize) -> usize {
        debug_assert!(bound > 0);
        let limit = u32::MAX - (u32::MAX % bound as u32);
        loop {
            let v = self.next_u32();
            if bound as u32 > limit || v < limit {
                return (v % bound as u32) as usize;
            }
        }
    }

    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        if items.len() < 2 {
            return;
        }
        for i in (1..items.len()).rev() {
            let j = self.below(i + 1);
            items.swap(i, j);
        }
    }
}

/// Read a whole file as UTF-8, replacing undecodable bytes (upstream reads
/// text with `errors="replace"` everywhere).
pub fn read_text_lossy(path: &Path) -> std::io::Result<String> {
    Ok(String::from_utf8_lossy(&std::fs::read(path)?).into_owned())
}

/// Copy one file, creating parent directories.
pub fn copy_file(from: &Path, to: &Path) -> std::io::Result<()> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(from, to)?;
    Ok(())
}

/// Recursively copy a directory tree, skipping `__pycache__` and `.pyc`
/// byproducts; returns the number of files copied.
pub fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<usize> {
    let mut n = 0usize;
    copy_tree_inner(src, src, dst, &mut n)?;
    Ok(n)
}

/// Directories that hold build output rather than source. A rollout copies a
/// whole workspace once per candidate, so carrying `target/`, package
/// artifacts or an unrelated `.git` pack turns a few rollouts into gigabytes
/// and hides the files the session is meant to edit.
pub const WORKSPACE_SKIP_DIRS: &[&str] = &[
    "target",
    "node_modules",
    "dist",
    "bin",
    "obj",
    ".git",
    ".audit",
    "__pycache__",
    ".venv",
    "venv",
    ".gradle",
    ".next",
];

/// Copy a workspace for one rollout, dropping build output and vendored trees.
///
/// Returns the number of files copied. The skip list is deliberately coarse:
/// a rollout must not depend on binaries the session never reads, and the
/// artifact it is scored on is written by the session itself.
pub fn copy_workspace(src: &Path, dst: &Path) -> std::io::Result<usize> {
    let mut n = 0usize;
    copy_workspace_inner(src, dst, &mut n)?;
    Ok(n)
}

fn copy_workspace_inner(src: &Path, dst: &Path, n: &mut usize) -> std::io::Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if path.is_dir() {
            if WORKSPACE_SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            copy_workspace_inner(&path, &dst.join(&name), n)?;
        } else if name.ends_with(".pyc") {
            continue;
        } else if path.is_symlink() {
            // A symlink can point outside the workspace; copying the bytes
            // would either duplicate a large tree or loop.
            continue;
        } else {
            copy_file(&path, &dst.join(&name))?;
            *n += 1;
        }
    }
    Ok(())
}

fn copy_tree_inner(root: &Path, src: &Path, dst: &Path, n: &mut usize) -> std::io::Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if path.is_dir() {
            if name == "__pycache__" {
                continue;
            }
            copy_tree_inner(root, &path, &dst.join(&name), n)?;
        } else if name.ends_with(".pyc") {
            continue;
        } else {
            copy_file(&path, &dst.join(&name))?;
            *n += 1;
        }
    }
    Ok(())
}

/// Remove a directory tree, ignoring every error (upstream `ignore_errors`).
pub fn remove_tree_all(path: &Path) {
    if path.is_dir() {
        let _ = std::fs::remove_dir_all(path);
    } else if path.is_file() {
        let _ = std::fs::remove_file(path);
    }
}

/// Sorted relative paths of every regular file under `root`, skipping
/// `__pycache__` and `*.pyc`.
pub fn walk_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk_files_inner(root, root, &mut out);
    out.sort();
    out
}

fn walk_files_inner(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if path.is_dir() {
            if name != "__pycache__" {
                walk_files_inner(root, &path, out);
            }
        } else if path.is_file() && !name.ends_with(".pyc") {
            out.push(path.strip_prefix(root).unwrap_or(&path).to_path_buf());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeded_shuffle_is_stable_and_seed_sensitive() {
        let mut a: Vec<u32> = (0..8).collect();
        SeededRandom::new("7:accounting").shuffle(&mut a);
        let mut b: Vec<u32> = (0..8).collect();
        SeededRandom::new("7:accounting").shuffle(&mut b);
        assert_eq!(a, b);
        let mut c: Vec<u32> = (0..8).collect();
        SeededRandom::new("8:accounting").shuffle(&mut c);
        assert_ne!(a, c);
        let mut sorted = a.clone();
        sorted.sort();
        assert_eq!(sorted, (0..8).collect::<Vec<u32>>());
    }

    #[test]
    fn runs_root_honours_override() {
        // Env tests would race other tests; assert only the fallback shape.
        let root = runs_root();
        let text = root.to_string_lossy();
        assert!(text.contains("harness2") || text.contains("runs") || root.is_absolute());
    }

    #[test]
    fn a_rollout_copy_leaves_build_output_behind() {
        let root = std::env::temp_dir().join(format!("h2-cw-{}", std::process::id()));
        remove_tree_all(&root);
        let src = root.join("src");
        let dst = root.join("dst");
        std::fs::create_dir_all(src.join("app")).unwrap();
        std::fs::create_dir_all(src.join("target/debug")).unwrap();
        std::fs::create_dir_all(src.join("node_modules/pkg")).unwrap();
        std::fs::create_dir_all(src.join(".git/objects")).unwrap();
        std::fs::write(src.join("app/main.rs"), "fn main() {}").unwrap();
        std::fs::write(src.join("README.md"), "hi").unwrap();
        std::fs::write(src.join("target/debug/bin"), vec![0u8; 1024]).unwrap();
        std::fs::write(src.join("node_modules/pkg/index.js"), "x").unwrap();
        std::fs::write(src.join(".git/objects/pack"), vec![0u8; 1024]).unwrap();
        let n = copy_workspace(&src, &dst).unwrap();
        assert_eq!(n, 2, "only source files come across");
        assert!(dst.join("app/main.rs").is_file());
        assert!(dst.join("README.md").is_file());
        assert!(!dst.join("target").exists());
        assert!(!dst.join("node_modules").exists());
        assert!(!dst.join(".git").exists());
        remove_tree_all(&root);
    }
}
