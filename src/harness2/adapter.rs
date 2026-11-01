//! The benchmark seam: everything a benchmark must supply, and nothing more.
//!
//! Three axes stay separate -- method (recursion, prompts, selection,
//! library), substrate (the agent framework a solver rollout runs under) and
//! benchmark (a [`BenchSpec`] value plus an [`Adapter`]).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use serde::{Deserialize, Serialize};

use super::tree::Tree;

pub type ModelId = String;

pub const CHANNELS: &[&str] = &["claude_cli", "opencode", "gnomeai"];

/// The benchmark-shaped text the shared prompts interpolate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchPrompts {
    pub work: String,
    #[serde(default = "default_domain_word")]
    pub domain_word: String,
    #[serde(default)]
    pub inv_levers: String,
    #[serde(default)]
    pub dom_levers: String,
    #[serde(default)]
    pub yardstick: String,
    #[serde(default = "default_deliverable_word")]
    pub deliverable_word: String,
    #[serde(default)]
    pub domain_guidance: String,
    #[serde(default)]
    pub selection_checks: String,
    #[serde(default)]
    pub selection_lead: String,
    #[serde(default = "default_conformance")]
    pub selection_conformance: String,
    #[serde(default = "default_pack")]
    pub prompt_pack: String,
}

fn default_domain_word() -> String {
    "domain".into()
}

fn default_deliverable_word() -> String {
    "deliverable".into()
}

fn default_conformance() -> String {
    "full".into()
}

fn default_pack() -> String {
    "adapt".into()
}

impl BenchPrompts {
    /// Validate the prompt pack: measurement language must never describe the
    /// work itself, so a grader-shaped slot cannot leak into model context.
    pub fn validate(&self) -> Result<(), String> {
        if self.prompt_pack != "adapt" && self.prompt_pack != "tts" {
            return Err(format!(
                "BenchPrompts.prompt_pack must be 'adapt' or 'tts', got {:?}",
                self.prompt_pack
            ));
        }
        let visible = [
            ("work", &self.work),
            ("inv_levers", &self.inv_levers),
            ("dom_levers", &self.dom_levers),
            ("yardstick", &self.yardstick),
            ("domain_guidance", &self.domain_guidance),
            ("selection_checks", &self.selection_checks),
        ];
        for (field_name, value) in visible {
            let low = value.to_lowercase();
            for bad in ["benchmark", "evaluation", "grader", "rubric", "score"] {
                if low.contains(bad) {
                    return Err(format!(
                        "BenchPrompts.{field_name} must describe the WORK, not its measurement \
                         (found {bad:?}). Say what the practitioner needs."
                    ));
                }
            }
        }
        Ok(())
    }
}

pub const INSTALL_MODES: &[&str] = &[
    "workspace_agents_md",
    "home_dir_jinja",
    "workspace_config",
    "codex_root",
];

/// Static description of one (benchmark, substrate) cell. A value, not a code
/// path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchSpec {
    pub name: String,
    pub runs_root: PathBuf,
    pub install_mode: String,
    pub prompts: BenchPrompts,
    #[serde(default)]
    pub domain_word: String,
    #[serde(default)]
    pub leak_globs: Vec<String>,
    #[serde(default)]
    pub allow_subagents: bool,
    #[serde(default)]
    pub proposer_deliverables: String,
    #[serde(default)]
    pub state_deliverable: bool,
    #[serde(default = "default_obs_trunc")]
    pub obs_trunc: usize,
    /// Bare script basenames the runtime provides, so proposals never need to
    /// ship them. Upstream `BenchSpec.reserved_scripts`.
    #[serde(default)]
    pub reserved_scripts: Vec<String>,
}

fn default_obs_trunc() -> usize {
    50_000
}

impl BenchSpec {
    pub fn validate(&self) -> Result<(), String> {
        if !INSTALL_MODES.contains(&self.install_mode.as_str()) {
            return Err(format!(
                "unknown install_mode {:?}; expected one of {INSTALL_MODES:?}",
                self.install_mode
            ));
        }
        let deliverables = self.effective_deliverables();
        if deliverables != "contents" && deliverables != "names" {
            return Err(format!(
                "BenchSpec.proposer_deliverables must be 'contents' or 'names', got {:?}",
                self.proposer_deliverables
            ));
        }
        if self.obs_trunc < 10_000 {
            return Err(format!(
                "obs_trunc={} is in the range that fabricates defects; use >= 10_000 (default \
                 50_000)",
                self.obs_trunc
            ));
        }
        let bad: Vec<String> = self
            .reserved_scripts
            .iter()
            .filter(|s| s.contains('/') || !s.ends_with(".py"))
            .cloned()
            .collect();
        if !bad.is_empty() {
            return Err(format!(
                "BenchSpec.reserved_scripts takes bare .py BASENAMES, not paths: {bad:?}"
            ));
        }
        self.prompts.validate()
    }

    pub fn effective_deliverables(&self) -> &str {
        // Empty means the default, `contents`.
        if self.proposer_deliverables.is_empty() {
            "contents"
        } else {
            &self.proposer_deliverables
        }
    }

    pub fn reserved_script_names(&self) -> &[String] {
        &self.reserved_scripts
    }
}

/// What the method knows about one finished rollout. Never carries a score.
#[derive(Debug, Clone, Serialize)]
pub struct RunView {
    pub label: String,
    pub run_id: String,
    pub run_dir: PathBuf,
    pub ok: bool,
    pub delivered: bool,
    #[serde(default)]
    pub cost_usd: f64,
    #[serde(default)]
    pub wall_s: f64,
    #[serde(default)]
    pub error: String,
}

impl RunView {
    pub fn dead(label: &str, run_id: &str, run_dir: PathBuf, error: &str) -> Self {
        Self {
            label: label.into(),
            run_id: run_id.into(),
            run_dir,
            ok: false,
            delivered: false,
            cost_usd: 0.0,
            wall_s: 0.0,
            error: error.into(),
        }
    }
}

/// The benchmark seam. Implement these and the whole method works; nothing in
/// the core benchmarks the adapter's internals. `execute` returns a boxed
/// future so the trait stays object-safe (`dyn Adapter`).
pub trait Adapter: Send + Sync {
    /// The static (benchmark, substrate) description.
    fn spec(&self) -> &BenchSpec;

    /// The surface name of the solver substrate (`opencode` | `codex` |
    /// `gnomeai`).
    fn substrate_name(&self) -> &str;

    /// Task ids for a split, in a stable order: library state must be
    /// reproducible.
    fn tasks(&self) -> Vec<String> {
        self.tasks_for_split("all")
    }

    fn tasks_for_split(&self, split: &str) -> Vec<String>;

    /// The grouping unit: practice area / occupation / role / sector.
    fn domain_of(&self, task_id: &str) -> String;

    /// The task as the agent received it, with criteria and expected output
    /// stripped.
    fn task_prompt(&self, task_id: &str) -> String;

    /// The source material the work was done against, when there is any.
    fn task_files(&self, task_id: &str) -> Option<PathBuf> {
        let _ = task_id;
        None
    }

    /// Run one rollout and return its run directory, idempotently by run_id.
    /// `harness_dir = None` means the unmodified base harness.
    fn execute<'a>(
        &'a self,
        task_id: &'a str,
        harness_dir: Option<&'a Path>,
        run_id: &'a str,
        timeout: Option<u64>,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<PathBuf>> + Send + 'a>>;

    /// Did this rollout produce the artifact the task asked for?
    fn delivered(&self, run_dir: &Path) -> bool;

    /// Readable trajectory in the shared `[ACTION]`/`[OBSERVATION]`/`[ERROR]`
    /// grammar.
    fn trajectory(&self, run_dir: &Path, obs_trunc: usize) -> anyhow::Result<String>;

    /// The artifact as text, extracted from agent-authored bytes only.
    fn deliverable_text(&self, run_dir: &Path) -> anyhow::Result<String>;

    /// One entry per artifact produced: filename -> extracted readable text.
    fn deliverable_files(&self, run_dir: &Path) -> Option<BTreeMap<String, String>> {
        let _ = run_dir;
        None
    }

    /// The unmodified c1..c8 tree this benchmark starts from.
    fn base_harness(&self) -> PathBuf;

    /// Benchmark-specific harness invariants. `Err` lists all problems in
    /// actionable prose; the message is fed back verbatim as retry feedback.
    fn validate_tree(&self, tree: &Tree) -> Result<(), String> {
        let _ = tree;
        Ok(())
    }

    /// Judge one run. `None` means the judge failed, which is not a zero and
    /// is never cached as one. Called only after selection committed.
    fn score(&self, run_dir: &Path, task_id: &str) -> Option<serde_json::Value>;
}

/// Convenience wrapper: the spec's obs_trunc, floored by the port's minimum.
pub fn trunc_for(adapter: &dyn Adapter) -> usize {
    adapter.spec().obs_trunc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prompts() -> BenchPrompts {
        serde_json::from_value(serde_json::json!({
            "work": "reconcile signed amounts in a transaction ledger"
        }))
        .unwrap()
    }

    #[test]
    fn prompt_slots_reject_measurement_language() {
        let mut p = prompts();
        assert!(p.validate().is_ok());
        p.work = "score the grader".into();
        assert!(p.validate().is_err());
    }

    #[test]
    fn spec_defaults_and_bounds() {
        let spec = BenchSpec {
            name: "demo".into(),
            runs_root: PathBuf::from("/tmp/x"),
            install_mode: "workspace_config".into(),
            prompts: prompts(),
            domain_word: String::new(),
            leak_globs: vec!["reference_answer.txt".into()],
            allow_subagents: false,
            proposer_deliverables: String::new(),
            state_deliverable: false,
            obs_trunc: 50_000,
            reserved_scripts: Vec::new(),
        };
        assert!(spec.validate().is_ok());
        // An empty value takes the documented default; validation never
        // rewrites the caller's value.
        assert_eq!(spec.effective_deliverables(), "contents");
        let mut bad = spec.clone();
        bad.obs_trunc = 100;
        assert!(bad.validate().is_err());
    }
}
