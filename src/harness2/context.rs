//! The evidence tree an agentic proposer reads. The only way evidence reaches
//! a model.
//!
//! ```text
//! context/task.md                  the task as the agent received it
//! context/rollouts/<label>.md      full trajectory -- no prompt-budget truncation
//! context/deliverables/<label>.md  the artifact, via the judge's own extractor
//! context/harness/<rel>            the c1..c8 tree being edited
//! context/library/INDEX.md         earlier tasks in this domain + their full harnesses
//! context/pool/<label>/            candidate harness + its rollout (aggregate / select only)
//! MISSION.md                       what to do with all of it
//! ```
//!
//! Evidence is served as files and the prompt is a short pointer, so nothing
//! is truncated to fit a prompt. [`mission`] builds that pointer.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::adapter::{Adapter, RunView};
use super::tree::{self, Tree};
use super::{copy_file, copy_tree, read_text_lossy, remove_tree_all, walk_files};

/// A built evidence tree, plus what actually landed in it.
#[derive(Debug, Clone)]
pub struct ContextTree {
    pub root: PathBuf,
    pub task_id: String,
    pub rollouts: Vec<String>,
    pub deliverables: Vec<String>,
    pub harness_files: usize,
    pub library: bool,
    pub pool: Vec<String>,
    pub failed: Vec<String>,
    pub deliverables_mode: String,
    pub source_files: usize,
    pub decisions: usize,
}

impl ContextTree {
    /// The evidence-layout listing that goes in the mission; only mentions
    /// what exists.
    pub fn layout(&self) -> String {
        let mut lines =
            vec!["context/task.md                  the task as the agent received it".to_string()];
        if self.source_files > 0 {
            lines.push("context/source/                 task source materials".to_string());
        }
        if self.decisions > 0 {
            lines.push(
                "context/decisions/              earlier edits and composition decisions"
                    .to_string(),
            );
        }
        if !self.rollouts.is_empty() {
            let shown: Vec<String> = self
                .rollouts
                .iter()
                .take(6)
                .map(|r| format!("{r}.md"))
                .collect();
            let more = if self.rollouts.len() > 6 {
                format!(" (+{} more)", self.rollouts.len() - 6)
            } else {
                String::new()
            };
            lines.push(format!(
                "context/rollouts/               FULL trajectories: {}{}",
                shown.join(", "),
                more
            ));
        }
        if !self.deliverables.is_empty() {
            if self.deliverables_mode == "names" {
                lines.push(
                    "context/deliverables/<label>/    INDEX.md lists WHICH files each rollout \
                     produced -- names only."
                        .to_string(),
                );
                lines.push(
                    "                                 File contents are deliberately not \
                     provided on this benchmark; work from the trajectories and the file set \
                     itself."
                        .to_string(),
                );
            } else {
                lines.push(
                    "context/deliverables/<label>/    the files each rollout produced, one per \
                     artifact (browse INDEX.md for the set)"
                        .to_string(),
                );
            }
        }
        if self.harness_files > 0 {
            lines.push(format!(
                "context/harness/                the harness you are editing ({} files)",
                self.harness_files
            ));
        }
        if !self.pool.is_empty() {
            lines.push(format!(
                "context/pool/                   {} candidates, each with its harness and its \
                 rollout",
                self.pool.len()
            ));
        }
        if self.library {
            lines.push(
                "context/library/                earlier tasks in this area -- browse INDEX.md \
                 first, then open what looks relevant"
                    .to_string(),
            );
        }
        lines.join("\n")
    }
}

/// One pool candidate handed to the builder: (label, harness dir, rollout).
#[derive(Debug, Clone)]
pub struct PoolEntry {
    pub label: String,
    pub harness_dir: Option<PathBuf>,
    pub rollout: Option<RunView>,
}

/// Write the evidence tree; return what landed, so the mission describes only
/// that. An adapter bug raised here is a stage error; only the trajectory and
/// deliverable readers are caught, because their failure is itself evidence.
#[allow(clippy::too_many_arguments)]
pub fn build(
    dest: &Path,
    adapter: &dyn Adapter,
    task_id: &str,
    rollouts: &[RunView],
    harness_dir: Option<&Path>,
    library_dir: Option<&Path>,
    pool: &[PoolEntry],
    decisions: &BTreeMap<String, String>,
    obs_trunc: usize,
) -> anyhow::Result<ContextTree> {
    if dest.exists() {
        remove_tree_all(dest);
    }
    std::fs::create_dir_all(dest)?;
    let names_only = adapter.spec().effective_deliverables() == "names";

    std::fs::write(
        dest.join("task.md"),
        format!("{}\n", adapter.task_prompt(task_id)),
    )?;
    let source_files = copy_source(adapter, task_id, dest)?;
    let mut n_decisions = 0usize;
    for (label, notes) in decisions {
        let path = dest.join("decisions").join(format!("{label}.md"));
        std::fs::create_dir_all(path.parent().unwrap())?;
        std::fs::write(
            &path,
            if notes.trim().is_empty() {
                "No decision notes returned."
            } else {
                notes.as_str()
            },
        )?;
        n_decisions += 1;
    }

    let mut got_r = Vec::new();
    let mut got_d = Vec::new();
    let mut failed_r = Vec::new();
    if !rollouts.is_empty() {
        std::fs::create_dir_all(dest.join("rollouts"))?;
        std::fs::create_dir_all(dest.join("deliverables"))?;
    }
    for r in rollouts {
        match write_rollout(dest, r, adapter, obs_trunc, names_only) {
            Ok(()) => got_r.push(r.label.clone()),
            Err(failure) => failed_r.push(failure),
        }
        got_d.push(r.label.clone());
    }

    let mut n_h = 0usize;
    if let Some(hd) = harness_dir {
        if hd.is_dir() {
            assert_no_leak_paths(hd, &adapter.spec().leak_globs)?;
            n_h = copy_tree(hd, &dest.join("harness"))?;
        }
    }

    let got_p = write_pool(dest, pool, adapter, obs_trunc, names_only)?;

    let mut lib = false;
    if let Some(ld) = library_dir {
        if ld.is_dir() && has_experience_files(ld) {
            assert_no_leak_paths(ld, &adapter.spec().leak_globs)?;
            copy_tree(ld, &dest.join("library"))?;
            lib = true;
        }
    }

    assert_no_leak_paths(dest, &adapter.spec().leak_globs)?;
    assert_no_grader_verdicts(dest)?;

    if !rollouts.is_empty() && got_r.is_empty() {
        anyhow::bail!(
            "no trajectory could be rendered for ANY rollout -- the proposer would be adapting \
             on nothing: {}",
            failed_r
                .iter()
                .take(4)
                .cloned()
                .collect::<Vec<_>>()
                .join("; ")
        );
    }
    Ok(ContextTree {
        root: dest.to_path_buf(),
        task_id: task_id.to_string(),
        rollouts: got_r,
        deliverables: got_d,
        harness_files: n_h,
        library: lib,
        pool: got_p,
        failed: failed_r,
        deliverables_mode: if names_only {
            "names".into()
        } else {
            "contents".into()
        },
        source_files,
        decisions: n_decisions,
    })
}

fn has_experience_files(dir: &Path) -> bool {
    walk_files(dir)
        .iter()
        .any(|p| p.file_name().map(|n| n == "EXPERIENCE.md").unwrap_or(false))
}

/// Copy the task's source material under `root`, minus answer-key paths.
pub fn copy_source(adapter: &dyn Adapter, task_id: &str, root: &Path) -> anyhow::Result<usize> {
    let Some(src) = adapter.task_files(task_id) else {
        return Ok(0);
    };
    if !src.is_dir() {
        return Ok(0);
    }
    let dst = root.join("source");
    let mut n = 0usize;
    for p in walk_files(&src) {
        let rel = p.to_string_lossy().replace('\\', "/");
        if is_leaked(&rel, &adapter.spec().leak_globs) {
            continue;
        }
        copy_file(&src.join(&p), &dst.join(&p))?;
        n += 1;
    }
    if n > 0 {
        assert_no_leak_paths(&dst, &adapter.spec().leak_globs)?;
    }
    Ok(n)
}

fn write_rollout(
    dest: &Path,
    r: &RunView,
    adapter: &dyn Adapter,
    trunc: usize,
    names_only: bool,
) -> Result<(), String> {
    let mut failure = String::new();
    let trajectory = match adapter.trajectory(&r.run_dir, trunc) {
        Ok(text) => text,
        Err(exc) => {
            failure = format!("{}: {exc}", r.label);
            format!("(trajectory unavailable: {exc})")
        }
    };
    let path = dest.join("rollouts").join(format!("{}.md", r.label));
    if let Err(exc) = std::fs::write(&path, format!("{trajectory}\n")) {
        return Err(format!("{}: write failed: {exc}", r.label));
    }

    let mut per_file: BTreeMap<String, String> = BTreeMap::new();
    if names_only {
        let _ = write_listing(
            &dest.join("deliverables").join(&r.label),
            &r.label,
            &listing(adapter, &r.run_dir),
        );
    } else if let Some(files) = adapter.deliverable_files(&r.run_dir) {
        per_file = files;
    }
    if per_file.is_empty() {
        if !names_only {
            let text = match adapter.deliverable_text(&r.run_dir) {
                Ok(t) if t.trim().is_empty() => "(no deliverable produced)".to_string(),
                Ok(t) => t,
                Err(exc) => format!("(deliverable unavailable: {exc})"),
            };
            let p = dest.join("deliverables").join(format!("{}.md", r.label));
            let _ = std::fs::write(&p, format!("{text}\n"));
        }
        if failure.is_empty() {
            Ok(())
        } else {
            Err(failure)
        }
    } else {
        let d = dest.join("deliverables").join(&r.label);
        let _ = std::fs::create_dir_all(&d);
        for (name, body) in &per_file {
            let safe = name.replace('/', "__");
            let safe = if safe.is_empty() {
                "unnamed".to_string()
            } else {
                safe
            };
            let file = if safe.ends_with(".txt") {
                d.join(&safe)
            } else {
                d.join(format!("{safe}.txt"))
            };
            let _ = std::fs::write(file, format!("{body}\n"));
        }
        let index = format!(
            "# {} produced {} file(s)\n\n{}\n\nEach `.txt` is the artifact converted to text \
             by the SAME reader the work is assessed through, so reason about correctness \
             from these -- an artifact that does not survive that conversion is treated as \
             unreadable however good it looks.\n",
            r.label,
            per_file.len(),
            per_file
                .iter()
                .map(|(n, b)| format!("- `{}`  ({} chars extracted)", n, b.len()))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let _ = std::fs::write(d.join("INDEX.md"), index);
        if failure.is_empty() {
            Ok(())
        } else {
            Err(failure)
        }
    }
}

fn write_pool(
    dest: &Path,
    pool: &[PoolEntry],
    adapter: &dyn Adapter,
    trunc: usize,
    names_only: bool,
) -> anyhow::Result<Vec<String>> {
    let mut got = Vec::new();
    for entry in pool {
        let d = dest.join("pool").join(&entry.label);
        std::fs::create_dir_all(&d)?;
        if let Some(cand_dir) = &entry.harness_dir {
            if cand_dir.is_dir() {
                assert_no_leak_paths(cand_dir, &adapter.spec().leak_globs)?;
                copy_tree(cand_dir, &d.join("harness"))?;
                let surface = super::substrate::surface_by_name(adapter.substrate_name())
                    .map_err(anyhow::Error::msg)?;
                let reference = tree::read_tree(&surface, &d.join("harness"));
                let candidate = tree::read_tree(&surface, cand_dir);
                let diff = unified_tree_diff(&reference, &candidate, &entry.label);
                std::fs::write(d.join("harness.diff"), diff)?;
            }
        }
        if let Some(roll) = &entry.rollout {
            let produced = match (adapter.trajectory(&roll.run_dir, trunc), names_only) {
                (Ok(t), _) => Some(t),
                (Err(_), _) => None,
            };
            match produced {
                Some(t) => {
                    std::fs::write(d.join("rollout.md"), format!("{t}\n"))?;
                    let body = if names_only {
                        listing_md(&entry.label, &listing(adapter, &roll.run_dir))
                    } else {
                        adapter
                            .deliverable_text(&roll.run_dir)
                            .unwrap_or_else(|_| "(nothing produced)".to_string())
                    };
                    std::fs::write(d.join("produced.md"), format!("{body}\n"))?;
                }
                None => {
                    std::fs::write(d.join("rollout.md"), "(unavailable)\n")?;
                }
            }
        }
        got.push(entry.label.clone());
    }
    Ok(got)
}

/// A unified diff between two tree states, in the standard +/- @@ form.
pub fn unified_tree_diff(reference: &Tree, candidate: &Tree, label: &str) -> String {
    let mut out = String::new();
    let mut names: Vec<&String> = reference.keys().chain(candidate.keys()).collect();
    names.sort();
    names.dedup();
    for name in names {
        let before = reference.get(name).map(|s| s.as_str()).unwrap_or("");
        let after = candidate.get(name).map(|s| s.as_str()).unwrap_or("");
        if before == after {
            continue;
        }
        out.push_str(&unified_lines(
            &format!("current/{name}"),
            &format!("{label}/{name}"),
            before,
            after,
        ));
    }
    out
}

/// A line-level unified diff of two texts (header lines included), LCS-based
/// with three lines of context around each hunk.
pub fn unified_lines(from: &str, to: &str, before: &str, after: &str) -> String {
    let a: Vec<&str> = before.lines().collect();
    let b: Vec<&str> = after.lines().collect();
    let n = a.len();
    let m = b.len();
    if n == 0 && m == 0 {
        return String::new();
    }
    let mut out = format!("--- {from}\n+++ {to}\n");
    let mut lcs = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                std::cmp::max(lcs[i + 1][j], lcs[i][j + 1])
            };
        }
    }
    let mut i = 0usize;
    let mut j = 0usize;
    // First pass: emit an edit script as (tag, a_index, b_index, line).
    enum Tag {
        Ctx,
        Del,
        Add,
    }
    let mut script: Vec<(Tag, usize, usize, &str)> = Vec::new();
    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] {
            script.push((Tag::Ctx, i, j, a[i]));
            i += 1;
            j += 1;
        } else if j >= m || (i < n && lcs[i + 1][j] >= lcs[i][j + 1]) {
            script.push((Tag::Del, i, j, a[i]));
            i += 1;
        } else {
            script.push((Tag::Add, i, j, b[j]));
            j += 1;
        }
    }

    // Second pass: group the script into hunks with <= 3 lines of context.
    const CTX: usize = 3;
    let mut hunks: Vec<String> = Vec::new();
    let mut k = 0usize;
    while k < script.len() {
        if matches!(script[k].0, Tag::Ctx) {
            k += 1;
            continue;
        }
        // Hunk starts CTX lines before the first change.
        let start = k.saturating_sub(CTX);
        // Extend while changes are within 2*CTX context lines of each other.
        let mut end = k;
        let mut last_change = k;
        let mut scan = k;
        while scan < script.len() {
            if !matches!(script[scan].0, Tag::Ctx) {
                if scan - last_change > 2 * CTX {
                    break;
                }
                end = scan;
                last_change = scan;
            }
            scan += 1;
        }
        let stop = std::cmp::min(script.len(), end + CTX + 1);
        let (mut sa, mut sb) = (usize::MAX, usize::MAX);
        let mut body = String::new();
        for (tag, ai, bi, line) in &script[start..stop] {
            match tag {
                Tag::Ctx => {
                    sa = sa.min(*ai);
                    sb = sb.min(*bi);
                    body.push(' ');
                }
                Tag::Del => {
                    sa = sa.min(*ai);
                    body.push('-');
                }
                Tag::Add => {
                    sb = sb.min(*bi);
                    body.push('+');
                }
            }
            body.push_str(line);
            body.push('\n');
        }
        let na = script[start..stop]
            .iter()
            .filter(|(t, ..)| !matches!(t, Tag::Add))
            .count();
        let nb = script[start..stop]
            .iter()
            .filter(|(t, ..)| !matches!(t, Tag::Del))
            .count();
        hunks.push(format!(
            "@@ -{},{} +{},{} @@\n{body}",
            sa + 1,
            na,
            sb + 1,
            nb
        ));
        k = stop;
    }
    out.push_str(&hunks.join(""));
    out
}

fn listing(adapter: &dyn Adapter, run_dir: &Path) -> Vec<String> {
    match adapter.deliverable_files(run_dir) {
        Some(files) => files.keys().cloned().collect(),
        None => match adapter.deliverable_text(run_dir) {
            Ok(_) => vec!["(deliverable text; no per-file listing implemented)".to_string()],
            Err(exc) => vec![format!("(listing unavailable: {exc})")],
        },
    }
}

const WITHHELD_NOTE: &str = "\
CONTENTS ARE WITHHELD ON THIS PATH, DELIBERATELY. You are shown WHICH files the rollout
produced and not what is inside them. This is not an oversight and not a size budget: on this
benchmark the extracted-deliverable tree lives inside the same trial directory as the grader's
own output, so serving file bodies here once served the grader's pass rate and failed-check
names along with them. Reason about the work from the TRAJECTORY -- what was attempted, what
was observed, what the run did with what it saw -- and from the file set itself, which is
evidence in its own right: the task names the artifacts it expects, so a missing or misnamed
file is a finding. Do not ask for the contents and do not infer that an unlisted file is empty.";

fn listing_md(label: &str, names: &[String]) -> String {
    format!(
        "# {label} produced {} file(s)\n\n{label}: {names:?}\n\n{}\n\n{WITHHELD_NOTE}\n",
        names.len(),
        names
            .iter()
            .map(|n| format!("- `{n}`"))
            .collect::<Vec<_>>()
            .join("\n")
    )
}

fn write_listing(dir: &Path, label: &str, names: &[String]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join("INDEX.md"), listing_md(label, names))
}

/// Whether `rel` matches one of the benchmark's answer-key globs (`*` wildcards).
pub fn is_leaked(rel: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|pattern| {
        let pat = pattern
            .replace('\\', "/")
            .trim_start_matches("./")
            .to_string();
        glob_match(&pat, rel) || {
            let nested = format!("*/{pat}");
            glob_match(&nested, rel)
        }
    })
}

/// A small `*`-glob matcher (the only wildcard the leak globs use).
fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    fn go(p: &[char], t: &[char]) -> bool {
        if p.is_empty() {
            return t.is_empty();
        }
        match p[0] {
            '*' => {
                for i in 0..=t.len() {
                    if go(&p[1..], &t[i..]) {
                        return true;
                    }
                }
                false
            }
            c => !t.is_empty() && t[0] == c && go(&p[1..], &t[1..]),
        }
    }
    go(&p, &t)
}

/// Reject benchmark-declared answer-key paths before a context reaches a model.
pub fn assert_no_leak_paths(root: &Path, patterns: &[String]) -> anyhow::Result<()> {
    if patterns.is_empty() || !root.is_dir() {
        return Ok(());
    }
    let mut hits = Vec::new();
    for rel in walk_files(root) {
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if is_leaked(&rel_str, patterns) {
            hits.push(rel_str);
        }
    }
    if hits.is_empty() {
        Ok(())
    } else {
        hits.sort();
        let shown: Vec<String> = hits.iter().take(8).cloned().collect();
        anyhow::bail!(
            "answer-key path(s) would enter model context: {}",
            shown.join(", ")
        )
    }
}

pub const GRADER_VERDICT_MARKERS: &[&str] = &[
    "deterministic_eval_summary",
    "deterministic_eval_result",
    "judge_scope_and_deterministic_status",
    "Judge findings on the rendered result",
    "\"passed_count\"",
    "\"pass_rate\"",
    "\"test_pass_rate\"",
];

const MARKER_SCAN_SUFFIXES: &[&str] = &[
    ".md", ".txt", ".json", ".csv", ".tsv", ".yaml", ".yml", ".html", ".htm", ".log", ".py", ".js",
    ".sh", ".xml", ".ini", ".toml", ".patch", ".diff", "",
];

fn scan_suffix(path: &str) -> bool {
    let lower = path.to_lowercase();
    // The list already carries the leading dot; an empty entry means "no
    // extension at all".
    MARKER_SCAN_SUFFIXES.iter().any(|suffix| {
        if suffix.is_empty() {
            !lower.rsplit('/').next().unwrap_or("").contains('.')
        } else {
            lower.ends_with(suffix)
        }
    })
}

/// Refuse a context tree containing the grader's own output, by file name and
/// by content. A tripwire on the adapters' read boundary, not the defence
/// itself: if it fires, find the read path that produced the match rather than
/// widening the marker list.
pub fn assert_no_grader_verdicts(root: &Path) -> anyhow::Result<()> {
    assert_no_grader_verdicts_with(root, GRADER_VERDICT_MARKERS)
}

pub fn assert_no_grader_verdicts_with(root: &Path, markers: &[&str]) -> anyhow::Result<()> {
    if !root.is_dir() {
        return Ok(());
    }
    for rel in walk_files(root) {
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        for m in markers {
            if rel_str.contains(m) {
                anyhow::bail!(
                    "grader verdict would enter model context: {rel_str:?} matched marker {m:?} \
                     in its NAME. The tripwire in harness2::context fired -- this is a leak, not \
                     a false positive to widen around. Find the read path that produced it."
                );
            }
        }
        if !scan_suffix(&rel_str) {
            continue;
        }
        let Ok(body) = read_text_lossy(&root.join(&rel)) else {
            continue;
        };
        for m in markers {
            if let Some(at) = body.find(m) {
                let lo = at.saturating_sub(60);
                let hi = std::cmp::min(body.len(), at + 120);
                anyhow::bail!(
                    "grader verdict would enter model context: {rel_str:?} contains marker \
                     {m:?} at offset {at} ({:?}). The tripwire in harness2::context fired -- \
                     this is a leak, not a false positive to widen around. Find the read path \
                     that produced it.",
                    &body[lo..hi]
                );
            }
        }
    }
    Ok(())
}

/// The same tripwire on an agent-authored harness tree (rel -> body), before
/// it can be rolled or stored: such a tree could never afterwards be served as
/// evidence.
pub fn assert_tree_has_no_grader_verdicts(tree: &Tree) -> Result<(), String> {
    assert_tree_has_no_grader_verdicts_with(tree, GRADER_VERDICT_MARKERS)
}

pub fn assert_tree_has_no_grader_verdicts_with(
    tree: &Tree,
    markers: &[&str],
) -> Result<(), String> {
    for (rel, body) in tree {
        for m in markers {
            if rel.contains(m) {
                return Err(format!(
                    "harness file {rel:?} has grader-verdict marker {m:?} in its NAME; rename \
                     it -- files carrying these strings cannot be served as evidence to later \
                     tasks"
                ));
            }
            if !scan_suffix(rel) {
                continue;
            }
            if let Some(at) = body.find(m) {
                let lo = at.saturating_sub(60);
                let hi = std::cmp::min(body.len(), at + 120);
                return Err(format!(
                    "harness file {rel:?} contains grader-verdict marker {m:?} at offset {at} \
                     ({:?}); remove or rename that string -- files carrying it cannot be \
                     served as evidence to later tasks",
                    &body[lo..hi]
                ));
            }
        }
    }
    Ok(())
}

/// The short pointer to the evidence on disk: the job, where the evidence is,
/// and the instruction to open it.
pub fn mission(tree: &ContextTree, job: &str) -> String {
    format!(
        "{}\n\n# YOUR EVIDENCE\n\nIt is on disk, in `context/`. Read it -- do not answer from \
         this message alone.\n\n{}\n\nEvery trajectory EVENT is present: the trajectory was \
         not clipped to fit this mission. An individual observation may contain an explicit \
         renderer-truncation marker; that marker is ours, not behavior by the solver. Do not \
         diagnose it as a solver failure.\n\nRead `task.md` and the current harness first. \
         Inventory every available rollout or candidate and inspect each at least once before \
         deciding. For a candidate, compare what changed from its editing baseline, what \
         behavior changed in its rollout, what improved, and what regressed or remained \
         unresolved. Then deep-read the evidence relevant to any edit. Cite the specific \
         trajectory moment behind every change; one you cannot trace to a moment is \
         speculation.\n",
        job.trim(),
        tree.layout()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_and_leak_paths() {
        let pats = vec!["reference_answer.txt".to_string()];
        assert!(is_leaked("reference_answer.txt", &pats));
        assert!(is_leaked("source/reference_answer.txt", &pats));
        assert!(!is_leaked("ledger.csv", &pats));
        assert!(is_leaked("x", &vec!["*.txt".to_string()]) == false);
        assert!(is_leaked("a.txt", &vec!["*.txt".to_string()]));
    }

    #[test]
    fn tree_diff_reports_changes() {
        let mut a = Tree::new();
        a.insert("f.md".into(), "one\ntwo\nthree\n".into());
        let mut b = a.clone();
        b.insert("f.md".into(), "one\nTWO\nthree\n".into());
        let d = unified_tree_diff(&a, &b, "cand");
        assert!(d.contains("--- current/f.md"));
        assert!(d.contains("+++ cand/f.md"));
        assert!(d.contains("-two"));
        assert!(d.contains("+TWO"));
    }
}
