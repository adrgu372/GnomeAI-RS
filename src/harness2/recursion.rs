//! Parallel and sequential task-level recursion.
//!
//! Each mode schedules 4k rollouts and 3k improvement stages, including
//! shared base executions. Invalid proposals receive one retry before
//! inheriting the current harness.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;

use super::adapter::{Adapter, RunView};
use super::context::{self, PoolEntry};
use super::ledger::Ledger;
use super::prompts as P;
use super::proposer::{Proposer, ProposerError};
use super::tree::{self, SurfaceSpec, Tree};
use super::{remove_tree_all, runs_root};

pub const STAGE_ATTEMPTS: u32 = 2;
pub const TRANSPORT_ATTEMPTS: u32 = 2;
pub const PHASE_WIDTH: usize = 2;

/// An improvement stage, including retries, its harness, and decision notes.
#[derive(Debug, Clone, Serialize)]
pub struct Stage {
    pub label: String,
    pub dim: String,
    pub tree: Tree,
    pub kept_base: bool,
    pub fell_back: bool,
    pub error: String,
    pub usd: f64,
    pub attempts: u32,
    pub cached: bool,
    pub notes: String,
}

impl Stage {
    fn empty(label: &str, dim: &str, fallback: &Tree) -> Self {
        Self {
            label: label.into(),
            dim: dim.into(),
            tree: fallback.clone(),
            kept_base: false,
            fell_back: false,
            error: String::new(),
            usd: 0.0,
            attempts: 0,
            cached: true,
            notes: String::new(),
        }
    }
}

/// One recursion mode's full record for one task.
#[derive(Debug, Serialize)]
pub struct ModeResult {
    pub mode: String,
    pub task_id: String,
    pub k: u32,
    pub stages: Vec<Stage>,
    pub trees: BTreeMap<String, Tree>,
    pub rollouts: BTreeMap<String, RunView>,
    pub finals: Vec<String>,
    pub ledger: serde_json::Value,
    pub similarity: serde_json::Value,
    pub errors: Vec<String>,
}

impl ModeResult {
    fn new(mode: &str, task_id: &str, k: u32) -> Self {
        Self {
            mode: mode.into(),
            task_id: task_id.into(),
            k,
            stages: Vec::new(),
            trees: BTreeMap::new(),
            rollouts: BTreeMap::new(),
            finals: Vec::new(),
            ledger: serde_json::Value::Null,
            similarity: serde_json::Value::Null,
            errors: Vec::new(),
        }
    }

    pub fn ok(&self) -> bool {
        !self.finals.is_empty() && self.errors.is_empty()
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "mode": self.mode,
            "task_id": self.task_id,
            "k": self.k,
            "finals": self.finals,
            "ok": self.ok(),
            "fell_back": self.stages.iter().filter(|s| s.fell_back).map(|s| s.label.clone()).collect::<Vec<_>>(),
            "kept_base": self.stages.iter().filter(|s| s.kept_base).map(|s| s.label.clone()).collect::<Vec<_>>(),
            "stages": self.stages,
            "similarity": self.similarity,
            "ledger": self.ledger,
            "errors": self.errors,
        })
    }
}

/// Repair then validate a proposed tree: (kept_base, "") if accepted, else
/// the error.
fn accept_tree(
    surface: &SurfaceSpec,
    adapter: &dyn Adapter,
    new: &mut Tree,
    fallback: &Tree,
    label: &str,
) -> (bool, String) {
    let triggered = tree::autotrigger(surface, new, None);
    if !triggered.is_empty() {
        eprintln!(
            "  [{label}] auto-triggered skill(s): {}",
            triggered.join(", ")
        );
    }
    let changed = tree::diff_summary(surface, fallback, new);
    let kept = changed.is_empty();
    let reserved_names = adapter.spec().reserved_script_names();
    let reserved = |base: &str| {
        tree::RESERVED_SCRIPTS.contains(&base) || reserved_names.iter().any(|n| n == base)
    };
    if let Err(exc) = tree::validate(
        surface,
        new,
        adapter.spec().allow_subagents,
        None,
        &reserved,
    ) {
        return (kept, exc);
    }
    if let Err(exc) = adapter.validate_tree(new) {
        return (kept, exc);
    }
    if let Err(exc) = context::assert_tree_has_no_grader_verdicts(new) {
        return (kept, exc);
    }
    (kept, String::new())
}

/// Build evidence, propose an edit, and retry an invalid proposal once.
#[allow(clippy::too_many_arguments)]
async fn run_stage(
    label: &str,
    dim: &str,
    job: &str,
    fallback: &Tree,
    adapter: &dyn Adapter,
    prop: &Proposer,
    ctx_dir: &Path,
    task_id: &str,
    rollouts: &[RunView],
    harness_dir: Option<&Path>,
    library_dir: Option<&Path>,
    pool: &[PoolEntry],
    decisions: &BTreeMap<String, String>,
) -> Stage {
    let surface = match super::substrate::surface_by_name(adapter.substrate_name()) {
        Ok(s) => s,
        Err(exc) => {
            let mut st = Stage::empty(label, dim, fallback);
            st.fell_back = true;
            st.error = format!("substrate: {exc}").chars().take(400).collect();
            return st;
        }
    };

    let built = context::build(
        &ctx_dir.join(label),
        adapter,
        task_id,
        rollouts,
        harness_dir,
        library_dir,
        pool,
        decisions,
        adapter.spec().obs_trunc,
    );
    let evidence = match built {
        Ok(tree) => tree,
        Err(exc) => {
            let mut st = Stage::empty(label, dim, fallback);
            st.fell_back = true;
            st.error = format!("could not build evidence: {exc}")
                .chars()
                .take(400)
                .collect();
            st.attempts = 0;
            return st;
        }
    };
    let mission = context::mission(&evidence, job);

    let mut err = String::new();
    let mut spent_usd = 0.0f64;
    let mut spent_calls = 0u32;
    let mut all_cached = true;
    for attempt in 1..=STAGE_ATTEMPTS {
        let full = if err.is_empty() {
            mission.clone()
        } else {
            format!(
                "{mission}\n\n# YOUR PREVIOUS ATTEMPT WAS REJECTED\n\n{err}\n\nFix it in \
                 `context/harness/` and reply again."
            )
        };
        let reply = prop
            .call(
                &full,
                Some(&evidence.root),
                &format!("{label}.a{attempt}"),
                "",
                Some(TRANSPORT_ATTEMPTS),
                Some("context/harness"),
            )
            .await;
        let reply = match reply {
            Ok(reply) => reply,
            Err(exc) => {
                err = exc.message.clone();
                spent_calls += if exc.attempts == 0 {
                    TRANSPORT_ATTEMPTS
                } else {
                    exc.attempts
                };
                spent_usd += exc.cost_usd;
                all_cached = false;
                break;
            }
        };
        spent_usd += reply.cost_usd;
        spent_calls += if reply.cached {
            0
        } else {
            reply.attempts.max(1)
        };
        all_cached = all_cached && reply.cached;

        let Some(workspace) = reply.workspace.clone() else {
            err = "the harness directory did not come back from the session; edit the files \
                   in `context/harness/` in place"
                .into();
            continue;
        };
        let mut rejected = Vec::new();
        let new = tree::read_tree_with_rejects(&surface, &workspace, Some(&mut rejected));
        if new.is_empty() {
            err = "`context/harness/` came back empty -- edit the files, do not delete them \
                   all"
            .into();
            remove_tree_all(&workspace);
            continue;
        }
        if !rejected.is_empty() {
            rejected.sort();
            rejected.dedup();
            let shown: Vec<String> = rejected.iter().take(6).cloned().collect();
            err = format!(
                "these files are off the editable surface and were not applied: {}. Component \
                 names must be lowercase letters, digits, `_` or `-` (e.g. `{}/doc_review/\
                 SKILL.md`, `scripts/check_totals.py`).",
                shown.join(", "),
                surface.skill_dir
            );
            remove_tree_all(&workspace);
            continue;
        }
        let mut new = new;
        let (kept, accept_err) = accept_tree(&surface, adapter, &mut new, fallback, label);
        if !accept_err.is_empty() {
            err = accept_err;
            // Persist the rejected tree beside the evidence for the retry.
            let _ = tree::write_tree(&surface, &new, &evidence.root.join("harness"), false);
            remove_tree_all(&workspace);
            continue;
        }
        remove_tree_all(&workspace);
        let notes_re = regex::Regex::new(r"(?s)<notes>(.*?)</notes>").expect("notes regex");
        let mut stage = Stage::empty(label, dim, fallback);
        stage.tree = new;
        stage.kept_base = kept;
        stage.usd = spent_usd;
        stage.attempts = spent_calls;
        stage.cached = all_cached;
        stage.notes = notes_re
            .captures(&reply.text)
            .map(|n| n[1].trim().to_string())
            .unwrap_or_else(|| reply.text.trim().to_string());
        return stage;
    }

    let mut stage = Stage::empty(label, dim, fallback);
    stage.fell_back = true;
    stage.error = err.chars().take(400).collect();
    stage.usd = spent_usd;
    stage.attempts = spent_calls;
    stage.cached = all_cached;
    stage
}

fn record_sessions(ledger: &mut Ledger, stages: &[Stage]) {
    for stage in stages {
        ledger.session(&stage.label, stage.usd, stage.cached, stage.attempts);
    }
}

/// Materialise a harness and roll it once. A dead rollout is recorded, never
/// raised.
async fn roll(
    adapter: Arc<dyn Adapter>,
    ledger: &mut Ledger,
    task_id: &str,
    label: &str,
    tree: Option<&Tree>,
    run_id: String,
    work_dir: &Path,
    surface: &SurfaceSpec,
    record: bool,
) -> RunView {
    let hdir: Option<PathBuf> = tree.map(|t| {
        let dir = work_dir.join("harnesses").join(label);
        let _ = tree::write_tree(surface, t, &dir, false);
        dir
    });
    let rv = match adapter
        .execute(task_id, hdir.as_deref(), &run_id, None)
        .await
    {
        Ok(rd) => {
            let exists = rd.exists();
            let delivered = exists && adapter.delivered(&rd);
            RunView {
                label: label.into(),
                run_id,
                run_dir: rd,
                ok: exists,
                delivered,
                cost_usd: 0.0,
                wall_s: 0.0,
                error: if exists {
                    String::new()
                } else {
                    "adapter returned a missing run directory".into()
                },
            }
        }
        Err(exc) => RunView {
            label: label.into(),
            run_id,
            run_dir: work_dir.join("missing"),
            ok: false,
            delivered: false,
            cost_usd: 0.0,
            wall_s: 0.0,
            error: format!("{exc}"),
        },
    };
    if record {
        ledger.rollout(label);
    }
    rv
}

/// Roll independent harnesses concurrently, then record them in the caller's
/// order. The ledger is written after the pool drains, so a concurrent run
/// records the same sequence of labels as a serial one.
async fn roll_many(
    adapter: Arc<dyn Adapter>,
    ledger: &mut Ledger,
    task_id: &str,
    items: Vec<(String, Tree)>,
    run_ids: BTreeMap<String, String>,
    work_dir: &Path,
    surface: &SurfaceSpec,
    width: usize,
) -> BTreeMap<String, RunView> {
    let mut out = BTreeMap::new();
    if items.is_empty() {
        return out;
    }
    let n = items.len().min(width).max(1);
    if n == 1 {
        for (label, tree) in &items {
            let rid = run_ids.get(label).cloned().unwrap_or_default();
            let rv = roll(
                adapter.clone(),
                ledger,
                task_id,
                label,
                Some(tree),
                rid,
                work_dir,
                surface,
                true,
            )
            .await;
            out.insert(label.clone(), rv);
        }
        return out;
    }
    let mut handles = Vec::new();
    for (label, tree) in &items {
        let adapter = adapter.clone();
        let label = label.clone();
        let tree = tree.clone();
        let rid = run_ids.get(&label).cloned().unwrap_or_default();
        let work_dir = work_dir.to_path_buf();
        let surface = surface.clone();
        let task_id_owned = task_id.to_string();
        handles.push(tokio::spawn(async move {
            let mut tmp_ledger = Ledger::new("batch", &task_id_owned, 0);
            let rv = roll(
                adapter,
                &mut tmp_ledger,
                &task_id_owned,
                &label,
                Some(&tree),
                rid,
                &work_dir,
                &surface,
                false,
            )
            .await;
            (label, rv)
        }));
    }
    let mut got = Vec::new();
    for handle in handles {
        if let Ok(pair) = handle.await {
            got.push(pair);
        }
    }
    for (label, tree) in &items {
        let rv = got
            .iter()
            .find(|(l, _)| l == label)
            .map(|(_, rv)| rv.clone())
            .unwrap_or_else(|| RunView {
                label: label.clone(),
                run_id: run_ids.get(label).cloned().unwrap_or_default(),
                run_dir: work_dir.join("missing"),
                ok: false,
                delivered: false,
                cost_usd: 0.0,
                wall_s: 0.0,
                error: "rollout task panicked".into(),
            });
        ledger.rollout(label);
        let _ = tree;
        out.insert(label.clone(), rv);
    }
    out
}

fn decisions_of(stages: &[Stage]) -> BTreeMap<String, String> {
    stages
        .iter()
        .map(|s| {
            (
                s.label.clone(),
                if s.notes.is_empty() {
                    s.error.clone()
                } else {
                    s.notes.clone()
                },
            )
        })
        .collect()
}

/// WIDTH: 2k iid candidates from shared evidence, then k INDEPENDENT
/// compositions.
#[allow(clippy::too_many_arguments)]
pub async fn run_parallel(
    task_id: &str,
    k: u32,
    base: &[RunView],
    adapter: Arc<dyn Adapter>,
    prop: Arc<Proposer>,
    work_dir: &Path,
    run_id_of: impl Fn(&str) -> String + Clone,
    base_tree: &Tree,
    library_dir: Option<&Path>,
    propose_workers: usize,
    rollout_workers: usize,
) -> anyhow::Result<ModeResult> {
    let surface =
        super::substrate::surface_by_name(adapter.substrate_name()).map_err(anyhow::Error::msg)?;
    let mut ledger = Ledger::new("agg", task_id, k);
    for r in base {
        ledger.rollout(&r.label);
    }
    let mut result = ModeResult::new("agg", task_id, k);
    for r in base {
        result.rollouts.insert(r.label.clone(), r.clone());
    }
    let ctx_dir = work_dir.join("context");
    let bp = adapter.spec().prompts.clone();
    let base_dir = work_dir.join("harnesses").join("base");
    tree::write_tree(&surface, base_tree, &base_dir, false)?;

    // Phase 1: 2k dimension-scoped proposals from shared base evidence.
    let mut specs: Vec<(String, &str)> = Vec::new();
    for i in 1..=k as usize {
        specs.push((format!("inv{i}"), "INV"));
        specs.push((format!("dom{i}"), "DOM"));
    }
    let mut stages: Vec<Stage> = Vec::new();
    let no_pool: Vec<PoolEntry> = Vec::new();
    let no_decisions: BTreeMap<String, String> = BTreeMap::new();
    for chunk in specs.chunks(propose_workers.max(1)) {
        // Own the jobs for the whole batch: the futures below only borrow.
        let mut batch: Vec<(String, &str, String)> = Vec::new();
        for (label, dim) in chunk {
            let index: usize = label[3..].parse().unwrap_or(1);
            let job = P::propose(&bp, dim, &surface, k as usize, index, None)
                .map_err(anyhow::Error::msg)?;
            batch.push((label.clone(), dim, job));
        }
        let futures = batch.iter().map(|(label, dim, job)| {
            run_stage(
                label,
                dim,
                job,
                base_tree,
                adapter.as_ref(),
                &prop,
                &ctx_dir,
                task_id,
                base,
                Some(&base_dir),
                library_dir,
                &no_pool,
                &no_decisions,
            )
        });
        stages.extend(futures_util::future::join_all(futures).await);
    }
    record_sessions(&mut ledger, &stages);
    for stage in &stages {
        result.stages.push(stage.clone());
        result.trees.insert(stage.label.clone(), stage.tree.clone());
    }

    // Phase 2: roll every candidate.
    let items: Vec<(String, Tree)> = stages
        .iter()
        .map(|s| (s.label.clone(), s.tree.clone()))
        .collect();
    let run_ids: BTreeMap<String, String> = items
        .iter()
        .map(|(label, _)| (label.clone(), run_id_of(label)))
        .collect();
    let rolled = roll_many(
        adapter.clone(),
        &mut ledger,
        task_id,
        items,
        run_ids,
        work_dir,
        &surface,
        rollout_workers,
    )
    .await;
    result.rollouts.extend(rolled);

    // Phase 3: k independent compositions.
    let pool_ev: Vec<PoolEntry> = stages
        .iter()
        .map(|s| PoolEntry {
            label: s.label.clone(),
            harness_dir: Some(work_dir.join("harnesses").join(&s.label)),
            rollout: result.rollouts.get(&s.label).cloned(),
        })
        .collect();
    let decisions = decisions_of(&stages);
    let agg_labels: Vec<String> = (1..=k as usize).map(|i| format!("agg{i}")).collect();
    let mut aggs: Vec<Stage> = Vec::new();
    for chunk in agg_labels.chunks(propose_workers.max(1)) {
        let mut batch: Vec<(String, String)> = Vec::new();
        for label in chunk {
            let index: usize = label[3..].parse().unwrap_or(1);
            let job = P::aggregate(&bp, &surface, k as usize, index).map_err(anyhow::Error::msg)?;
            batch.push((label.clone(), job));
        }
        let futures = batch.iter().map(|(label, job)| {
            run_stage(
                label,
                "AGG",
                job,
                base_tree,
                adapter.as_ref(),
                &prop,
                &ctx_dir,
                task_id,
                base,
                Some(&base_dir),
                library_dir,
                &pool_ev,
                &decisions,
            )
        });
        aggs.extend(futures_util::future::join_all(futures).await);
    }
    record_sessions(&mut ledger, &aggs);
    let mut agg_items = Vec::new();
    for stage in &aggs {
        result.stages.push(stage.clone());
        result.trees.insert(stage.label.clone(), stage.tree.clone());
        result.finals.push(stage.label.clone());
        agg_items.push((stage.label.clone(), stage.tree.clone()));
    }
    let agg_run_ids: BTreeMap<String, String> = agg_items
        .iter()
        .map(|(label, _)| (label.clone(), run_id_of(label)))
        .collect();
    let rolled = roll_many(
        adapter.clone(),
        &mut ledger,
        task_id,
        agg_items,
        agg_run_ids,
        work_dir,
        &surface,
        rollout_workers,
    )
    .await;
    result.rollouts.extend(rolled);

    let trees: BTreeMap<String, Tree> = aggs
        .iter()
        .map(|s| (s.label.clone(), s.tree.clone()))
        .collect();
    result.similarity = tree::diversity(&trees);
    ledger
        .assert_identity()
        .map_err(|e| anyhow::anyhow!("budget identity failed: {e}"))?;
    result.ledger = ledger.to_json();
    Ok(result)
}

/// DEPTH: k nested passes over the same shared base evidence.
#[allow(clippy::too_many_arguments)]
pub async fn run_sequential(
    task_id: &str,
    k: u32,
    base: &[RunView],
    adapter: Arc<dyn Adapter>,
    prop: Arc<Proposer>,
    work_dir: &Path,
    run_id_of: impl Fn(&str) -> String + Clone,
    base_tree: &Tree,
    library_dir: Option<&Path>,
    propose_workers: usize,
    rollout_workers: usize,
) -> anyhow::Result<ModeResult> {
    let surface =
        super::substrate::surface_by_name(adapter.substrate_name()).map_err(anyhow::Error::msg)?;
    let mut ledger = Ledger::new("seq", task_id, k);
    for r in base {
        ledger.rollout(&r.label);
    }
    let mut result = ModeResult::new("seq", task_id, k);
    for r in base {
        result.rollouts.insert(r.label.clone(), r.clone());
    }
    let ctx_dir = work_dir.join("context");
    let bp = adapter.spec().prompts.clone();

    let mut current_harness = base_tree.clone();
    let mut history: Vec<PoolEntry> = Vec::new();
    for t in 1..=k as usize {
        let current_label = if t > 1 {
            format!("c{}", t - 1)
        } else {
            "base".to_string()
        };
        let current_dir = work_dir.join("harnesses").join(&current_label);
        tree::write_tree(&surface, &current_harness, &current_dir, false)?;

        let specs = vec![(format!("inv{t}"), "INV"), (format!("dom{t}"), "DOM")];
        let mut stages: Vec<Stage> = Vec::new();
        for (label, dim) in &specs {
            let job = P::propose(&bp, dim, &surface, k as usize, t, (k > 1).then_some(t))
                .map_err(anyhow::Error::msg)?;
            stages.push(
                run_stage(
                    label,
                    dim,
                    &job,
                    &current_harness,
                    adapter.as_ref(),
                    &prop,
                    &ctx_dir,
                    task_id,
                    base,
                    Some(&current_dir),
                    library_dir,
                    &history,
                    &decisions_of(&result.stages),
                )
                .await,
            );
        }
        let _ = propose_workers; // sequential rounds propose as a pair, width 2
        record_sessions(&mut ledger, &stages);
        for stage in &stages {
            result.stages.push(stage.clone());
            result.trees.insert(stage.label.clone(), stage.tree.clone());
        }
        let items: Vec<(String, Tree)> = stages
            .iter()
            .map(|s| (s.label.clone(), s.tree.clone()))
            .collect();
        let run_ids: BTreeMap<String, String> = items
            .iter()
            .map(|(label, _)| (label.clone(), run_id_of(label)))
            .collect();
        let rolled = roll_many(
            adapter.clone(),
            &mut ledger,
            task_id,
            items,
            run_ids,
            work_dir,
            &surface,
            rollout_workers,
        )
        .await;
        result.rollouts.extend(rolled);

        let mut round_pool = history.clone();
        for stage in &stages {
            round_pool.push(PoolEntry {
                label: stage.label.clone(),
                harness_dir: Some(work_dir.join("harnesses").join(&stage.label)),
                rollout: result.rollouts.get(&stage.label).cloned(),
            });
        }
        let ck = format!("c{t}");
        let comp_job = if k == 1 {
            P::aggregate(&bp, &surface, 1, 1).map_err(anyhow::Error::msg)?
        } else {
            P::compose_round(&bp, &surface, t, k as usize).map_err(anyhow::Error::msg)?
        };
        let comp = run_stage(
            &ck,
            "COMPOSE",
            &comp_job,
            &current_harness,
            adapter.as_ref(),
            &prop,
            &ctx_dir,
            task_id,
            base,
            Some(&current_dir),
            library_dir,
            &round_pool,
            &decisions_of(&result.stages),
        )
        .await;
        record_sessions(&mut ledger, &[comp.clone()]);
        result.stages.push(comp.clone());
        result.trees.insert(ck.clone(), comp.tree.clone());
        let ck_rv = roll(
            adapter.clone(),
            &mut ledger,
            task_id,
            &ck,
            Some(&comp.tree),
            run_id_of(&ck),
            work_dir,
            &surface,
            true,
        )
        .await;
        result.rollouts.insert(ck.clone(), ck_rv);

        round_pool.push(PoolEntry {
            label: ck.clone(),
            harness_dir: Some(work_dir.join("harnesses").join(&ck)),
            rollout: result.rollouts.get(&ck).cloned(),
        });
        history = round_pool;
        current_harness = comp.tree.clone();
    }

    result.finals = (1..=k as usize).map(|i| format!("c{i}")).collect();
    ledger
        .assert_identity()
        .map_err(|e| anyhow::anyhow!("budget identity failed: {e}"))?;
    result.ledger = ledger.to_json();
    Ok(result)
}

/// Dispatch on the on-disk mode token: "agg" (parallel) or "seq"
/// (sequential).
#[allow(clippy::too_many_arguments)]
pub async fn run_mode(
    mode: &str,
    task_id: &str,
    k: u32,
    base: &[RunView],
    adapter: Arc<dyn Adapter>,
    prop: Arc<Proposer>,
    work_dir: &Path,
    run_id_of: impl Fn(&str) -> String + Clone,
    base_tree: &Tree,
    library_dir: Option<&Path>,
) -> anyhow::Result<ModeResult> {
    let _ = runs_root();
    match mode {
        "agg" => {
            run_parallel(
                task_id,
                k,
                base,
                adapter,
                prop,
                work_dir,
                run_id_of,
                base_tree,
                library_dir,
                PHASE_WIDTH,
                PHASE_WIDTH,
            )
            .await
        }
        "seq" => {
            run_sequential(
                task_id,
                k,
                base,
                adapter,
                prop,
                work_dir,
                run_id_of,
                base_tree,
                library_dir,
                2,
                PHASE_WIDTH,
            )
            .await
        }
        other => Err(anyhow::anyhow!(
            "unknown mode {other:?}; the method has exactly two: 'agg' (parallel) and 'seq' \
             (sequential)"
        )),
    }
}

/// ProposerError re-export used by callers matching on failures.
pub type StageError = ProposerError;
