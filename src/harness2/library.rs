//! The per-domain experience library R: what earlier tasks in this domain
//! produced.
//!
//! ```text
//! <root>/<mode>/<domain>/INDEX.md
//! <root>/<mode>/<domain>/harnesses/<task>/harness/<rel>   the full adopted tree
//! <root>/<mode>/<domain>/harnesses/<task>/EXPERIENCE.md   why picked + lesson
//! ```
//!
//! Retrieval is by construction: a domain's directory only ever holds
//! same-domain entries. The whole tree is carried, never a summary.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::context;
use super::read_text_lossy;
use super::tree::{self, SurfaceSpec, Tree};
use super::{remove_tree_all, walk_files};

/// The four evidence-supported fields a lesson must carry.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct Experience {
    #[serde(default)]
    pub cue: String,
    #[serde(default)]
    pub lesson: String,
    #[serde(default)]
    pub evidence: String,
    #[serde(default)]
    pub confidence: String,
    /// Free-form record of what each edit did; carried, never parsed.
    #[serde(default)]
    pub edits: String,
}

impl Experience {
    pub fn has_required_fields(&self) -> bool {
        !self.cue.trim().is_empty()
            && !self.lesson.trim().is_empty()
            && !self.evidence.trim().is_empty()
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "cue": self.cue,
            "lesson": self.lesson,
            "evidence": self.evidence,
            "confidence": self.confidence,
            "edits": self.edits,
        })
    }
}

pub fn slug(task_id: &str) -> String {
    task_id.replace('/', "__")
}

/// One directory name for a domain, never a path: only separators and the
/// whitespace around them collapse, so a domain without one maps to itself.
pub fn domain_dir(domain: &str) -> String {
    let replaced = regex::Regex::new(r"\s*[/\\]\s*")
        .expect("domain regex")
        .replace_all(domain, "__")
        .into_owned();
    let trimmed = replaced.trim();
    if trimmed.is_empty() {
        "unknown".into()
    } else {
        trimmed.to_string()
    }
}

/// One (run, domain) library: read before each task, appended after.
#[derive(Clone)]
pub struct Library {
    pub root: PathBuf,
    pub domain: String,
    pub surface_name: &'static str,
}

impl Library {
    pub fn new(root: impl Into<PathBuf>, domain: &str, surface_name: &'static str) -> Self {
        Self {
            root: root.into(),
            domain: domain.into(),
            surface_name,
        }
    }

    pub fn dir(&self) -> PathBuf {
        self.root.join(domain_dir(&self.domain))
    }

    pub fn index(&self) -> PathBuf {
        self.dir().join("INDEX.md")
    }

    pub fn entry_dir(&self, task_id: &str) -> PathBuf {
        self.dir().join("harnesses").join(slug(task_id))
    }

    pub fn exists(&self) -> bool {
        self.index().is_file()
    }

    /// The directory handed to an improver session, or None when this domain
    /// is empty.
    pub fn context_dir(&self) -> Option<PathBuf> {
        if self.exists() && has_experience_md(&self.dir()) {
            Some(self.dir())
        } else {
            None
        }
    }

    pub fn entries(&self) -> Vec<String> {
        let d = self.dir().join("harnesses");
        let mut names: Vec<String> = match std::fs::read_dir(&d) {
            Ok(rd) => rd
                .flatten()
                .filter(|e| e.path().is_dir())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect(),
            Err(_) => Vec::new(),
        };
        names.sort();
        names
    }

    pub fn tree_of(&self, surface: &SurfaceSpec, task_id: &str) -> Tree {
        tree::read_tree(surface, &self.entry_dir(task_id).join("harness"))
    }

    /// Drop the entries for these tasks and rebuild INDEX.md; unknown tasks
    /// are ignored.
    pub fn forget(&self, task_ids: &[String]) {
        let mut removed = false;
        for tid in task_ids {
            let d = self.entry_dir(tid);
            if d.is_dir() {
                remove_tree_all(&d);
                removed = true;
            }
        }
        if removed {
            let _ = self.rebuild_index();
        }
    }

    /// Keep only the entries preceding the current task in this stream.
    pub fn retain(&self, task_ids: &[String]) {
        let keep: std::collections::BTreeSet<String> = task_ids.iter().map(|t| slug(t)).collect();
        let drop: Vec<String> = self
            .entries()
            .into_iter()
            .filter(|e| !keep.contains(e))
            .collect();
        if !drop.is_empty() {
            self.forget(&drop);
        }
    }

    /// Store one adopted harness plus why it was adopted, and rebuild INDEX.md
    /// from disk. The tree is checked for grader verdicts before anything is
    /// written.
    pub fn add(
        &self,
        surface: &SurfaceSpec,
        task_id: &str,
        harness: &Tree,
        why: &str,
        label: &str,
        run_id: &str,
        experience: Option<&Experience>,
    ) -> anyhow::Result<Option<PathBuf>> {
        let Some(exp) = experience else {
            return Ok(None);
        };
        if !exp.has_required_fields() {
            return Ok(None);
        }
        context::assert_tree_has_no_grader_verdicts(harness).map_err(anyhow::Error::msg)?;
        let mut experience_marker = Tree::new();
        experience_marker.insert("EXPERIENCE.md".into(), format!("{}{why}", exp.to_json()));
        context::assert_tree_has_no_grader_verdicts(&experience_marker)
            .map_err(anyhow::Error::msg)?;
        let d = self.entry_dir(task_id);
        remove_tree_all(&d.join("harness"));
        tree::write_tree(surface, harness, &d.join("harness"), false)?;

        let mut lines = vec![
            format!("# {task_id}"),
            String::new(),
            format!("- adopted: `{label}`")
                + &if run_id.is_empty() {
                    String::new()
                } else {
                    format!("  (run `{run_id}`)")
                },
            format!("- components: {}", inventory(surface, harness)),
            String::new(),
        ];
        if !why.trim().is_empty() {
            lines.push("## Why this was chosen".into());
            lines.push(why.trim().to_string());
            lines.push(String::new());
        }
        if !exp.edits.trim().is_empty() {
            lines.push("## What each edit did".into());
            lines.push(exp.edits.trim().to_string());
            lines.push(String::new());
        }
        if !exp.lesson.trim().is_empty() {
            lines.push("## Transferable lesson".into());
            for (field, value) in [
                ("cue", &exp.cue),
                ("lesson", &exp.lesson),
                ("evidence", &exp.evidence),
                ("confidence", &exp.confidence),
            ] {
                if !value.trim().is_empty() {
                    lines.push(format!("- **{field}**: {}", value.trim()));
                }
            }
            lines.push(String::new());
        }
        std::fs::create_dir_all(&d)?;
        std::fs::write(d.join("EXPERIENCE.md"), lines.join("\n"))?;
        self.rebuild_index()?;
        Ok(Some(d))
    }

    /// Regenerate INDEX.md from what is on disk.
    pub fn rebuild_index(&self) -> anyhow::Result<()> {
        std::fs::create_dir_all(self.dir())?;
        let mut rows = vec![format!(
            "# Library: {}\n\nAdopted harnesses from earlier tasks in this domain, with the \
             reason each was chosen. Browse this index first, then open the entries that look \
             relevant.\n",
            self.domain
        )];
        for entry in self.entries() {
            let d = self.dir().join("harnesses").join(&entry);
            let experience = read_text_lossy(&d.join("EXPERIENCE.md")).unwrap_or_default();
            let lesson = experience
                .lines()
                .skip_while(|l| !l.starts_with("## Transferable lesson"))
                .find(|l| l.starts_with("- **lesson**"))
                .unwrap_or("")
                .to_string();
            let why = experience
                .lines()
                .skip_while(|l| !l.starts_with("## Why this was chosen"))
                .nth(1)
                .unwrap_or("")
                .to_string();
            rows.push(format!(
                "## {entry}\n\n- lesson: {lesson}\n- why: {}\n- harness: \
                 harnesses/{entry}/harness/\n",
                why.trim()
            ));
        }
        std::fs::write(self.index(), rows.join("\n"))?;
        Ok(())
    }
}

fn has_experience_md(dir: &Path) -> bool {
    walk_files(dir)
        .iter()
        .any(|p| p.file_name().map(|n| n == "EXPERIENCE.md").unwrap_or(false))
}

/// A one-line census of the components a harness carries.
pub fn inventory(surface: &SurfaceSpec, tree: &Tree) -> String {
    let skills = tree::skill_names(surface, tree);
    let subs = tree::subagent_names(tree);
    let scripts = tree.keys().filter(|r| tree::script_of(r)).count();
    let plugins = surface
        .plugin_dir
        .map(|dir| {
            tree.keys()
                .filter(|r| {
                    r.strip_prefix(dir)
                        .and_then(|x| x.strip_prefix('/'))
                        .map(|f| f.ends_with(".ts"))
                        .unwrap_or(false)
                })
                .count()
        })
        .unwrap_or(0);
    format!(
        "{} skill(s), {} sub-agent(s), {} script(s), {} plugin(s)",
        skills.len(),
        subs.len(),
        scripts,
        plugins
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness2::substrate::opencode_surface;

    fn exp() -> Experience {
        serde_json::from_value(serde_json::json!({
            "cue": "when a ledger includes credits and debits",
            "lesson": "include both signs when summing in scripts/summarize.py",
            "evidence": "the summarize.py row -- removing the filter changed the total",
            "confidence": "high",
        }))
        .unwrap()
    }

    #[test]
    fn domain_names_collapse_separators() {
        assert_eq!(domain_dir("a / b"), "a__b");
        assert_eq!(domain_dir("solo"), "solo");
        assert_eq!(domain_dir("   "), "unknown");
    }

    #[test]
    fn experience_requires_all_fields() {
        assert!(exp().has_required_fields());
        let thin: Experience = serde_json::from_value(serde_json::json!({"cue": "x"})).unwrap();
        assert!(!thin.has_required_fields());
    }

    #[test]
    fn add_retain_and_index_round_trip() {
        let surface = opencode_surface();
        let root = std::env::temp_dir().join(format!("h2-lib-{}", std::process::id()));
        remove_tree_all(&root);
        let lib = Library::new(root.join("agg"), "accounting", "opencode");
        let mut harness = Tree::new();
        harness.insert("systemprompt.md".into(), "Be careful with totals.".into());
        harness.insert("guardrails.md".into(), String::new());
        harness.insert(
            "LongTermMEMORY.md".into(),
            "In this workspace, amounts carry signs.\n".into(),
        );
        harness.insert("tool_descriptions/tool_guidance.md".into(), String::new());
        let added = lib
            .add(
                &surface,
                "ledger-1",
                &harness,
                "it won",
                "agg1",
                "run-9",
                Some(&exp()),
            )
            .unwrap()
            .unwrap();
        assert!(added.join("EXPERIENCE.md").is_file());
        assert!(lib.exists());
        assert_eq!(lib.entries(), vec!["ledger-1".to_string()]);
        let got = lib.tree_of(&surface, "ledger-1");
        assert_eq!(got["systemprompt.md"], harness["systemprompt.md"]);
        // Without an experience, nothing is written.
        let none = lib
            .add(&surface, "ledger-2", &harness, "w", "l", "r", None)
            .unwrap();
        assert!(none.is_none());
        // Retain drops unknown entries.
        lib.retain(&["ledger-9".to_string()]);
        assert!(lib.entries().is_empty());
        remove_tree_all(&root);
    }
}
