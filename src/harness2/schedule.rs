//! Namespacing, the seeded stream order, and the drivers: one task, one
//! domain, a sweep.
//!
//! `ns()` is the only function that builds a run namespace; the `Namespace`
//! struct is the only thing that globs over one. Scheme:
//! `<bench>-<substrate>-<mode><k>[-s<seed>][-<tag>]`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Serialize;

use super::adapter::{Adapter, RunView};
use super::library::{self, Library};
use super::recursion;
use super::runmark;
use super::selection;
use super::tree::{self, Tree};

pub const MODES: &[&str] = &["agg", "seq"];

/// The on-disk token for a mode name: parallel/agg -> "agg", sequential/seq
/// -> "seq".
pub fn mode_token(name: &str) -> Result<String, String> {
    match name.trim().to_lowercase().as_str() {
        "agg" | "parallel" => Ok("agg".into()),
        "seq" | "sequential" => Ok("seq".into()),
        other => Err(format!(
            "unknown mode {other:?}; expected parallel | sequential (or the on-disk tokens \
             {MODES:?})"
        )),
    }
}

/// Check a tag's shape: it becomes a path segment and must not spell a seed
/// segment.
pub fn validate_tag(tag: &str) -> Result<(), String> {
    if tag.is_empty() {
        return Ok(());
    }
    let ok_first = tag
        .chars()
        .next()
        .map(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
        .unwrap_or(false);
    let ok_rest = tag
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
    if !ok_first || !ok_rest {
        return Err(format!(
            "tag {tag:?} must be [A-Za-z0-9_.-] with no leading '-' and no '/': it becomes a \
             path segment and a run-id segment"
        ));
    }
    if regex::Regex::new(r"^s\d+(-|$)")
        .expect("seed regex")
        .is_match(tag)
    {
        return Err(format!(
            "tag {tag:?} spells a seed segment; pass --seed instead so seeded and tagged \
             namespaces can never collide"
        ));
    }
    Ok(())
}

/// Every name for one (benchmark, substrate, mode, k, seed, tag) setting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Namespace {
    pub benchmark: String,
    pub substrate: String,
    pub mode: String,
    pub k: u32,
    pub seed: Option<u64>,
    pub tag: String,
}

impl Namespace {
    pub fn new(
        benchmark: &str,
        substrate: &str,
        mode: &str,
        k: u32,
        seed: Option<u64>,
        tag: &str,
    ) -> Self {
        Self {
            benchmark: benchmark.into(),
            substrate: substrate.into(),
            mode: mode.into(),
            k,
            seed,
            tag: tag.into(),
        }
    }

    pub fn token(&self) -> String {
        ns(
            &self.benchmark,
            &self.substrate,
            &self.mode,
            self.k,
            self.seed,
            &self.tag,
        )
        .expect("namespace token")
    }

    pub fn run_id(&self, task_id: &str, label: &str) -> String {
        format!("{}/{task_id}/{label}", self.token())
    }

    pub fn task_dir(&self, root: &Path, task_id: &str) -> PathBuf {
        root.join(self.token()).join(task_id.replace('/', "__"))
    }
}

/// The only constructor of a run namespace string.
pub fn ns(
    benchmark: &str,
    substrate: &str,
    mode: &str,
    k: u32,
    seed: Option<u64>,
    tag: &str,
) -> Result<String, String> {
    if !MODES.contains(&mode) {
        return Err(format!(
            "unknown mode {mode:?}; the method has exactly two: {MODES:?}"
        ));
    }
    if substrate.is_empty() {
        return Err(
            "namespace needs a substrate name ('oc' | 'cx' | 'ga'); an empty one would let \
             the substrates' caches collide on every label"
                .into(),
        );
    }
    validate_tag(tag)?;
    let mut out = format!("{benchmark}-{substrate}-{mode}{k}");
    if let Some(s) = seed {
        out.push_str(&format!("-s{s}"));
    }
    if !tag.is_empty() {
        out.push_str(&format!("-{tag}"));
    }
    Ok(out)
}

/// The full (domain, task_id) stream, the one constructor of continual
/// learning order. seed=None is the reference order: `reference_domain_order`
/// with tasks sorted. A seed shuffles both levels deterministically, starting
/// from SORTED input, so the result depends only on (seed, domain, task set).
pub fn stream_order(
    tasks_by_domain: &BTreeMap<String, Vec<String>>,
    seed: Option<u64>,
    reference_domain_order: &[String],
) -> Result<Vec<(String, String)>, String> {
    let domains: Vec<String> = tasks_by_domain.keys().cloned().collect();
    let ordered_domains: Vec<String> = match seed {
        None => {
            let known: std::collections::BTreeSet<&String> =
                reference_domain_order.iter().collect();
            let missing: Vec<String> = domains
                .iter()
                .filter(|d| !known.contains(d))
                .cloned()
                .collect();
            if !missing.is_empty() {
                return Err(format!(
                    "reference_domain_order does not cover domain(s) {missing:?}; the frozen \
                     order list is stale -- regenerate it rather than letting an unlisted \
                     domain land in an arbitrary position"
                ));
            }
            reference_domain_order
                .iter()
                .filter(|d| tasks_by_domain.contains_key(*d))
                .cloned()
                .collect()
        }
        Some(s) => {
            let mut d = domains;
            let mut rng = super::SeededRandom::new(&format!("{s}:__domains__"));
            rng.shuffle(&mut d);
            d
        }
    };
    let mut out = Vec::new();
    for d in ordered_domains {
        let Some(tasks) = tasks_by_domain.get(&d) else {
            continue;
        };
        let mut tasks = tasks.clone();
        tasks.sort();
        if let Some(s) = seed {
            let mut rng = super::SeededRandom::new(&format!("{s}:{d}"));
            rng.shuffle(&mut tasks);
        }
        out.extend(tasks.into_iter().map(|t| (d.clone(), t)));
    }
    Ok(out)
}

/// The count-then-name default domain order.
pub fn default_reference_domain_order(by_domain: &BTreeMap<String, Vec<String>>) -> Vec<String> {
    let mut rows: Vec<(usize, String)> = by_domain
        .iter()
        .map(|(d, tasks)| (usize::MAX - tasks.len(), d.clone()))
        .collect();
    rows.sort();
    rows.into_iter().map(|(_, d)| d).collect()
}

fn tree_is_base(surface: &super::tree::SurfaceSpec, base: &Tree, candidate: &Tree) -> bool {
    tree::diff_summary(surface, base, candidate).is_empty()
}

/// One task's full record.
#[derive(Debug, Clone, Serialize)]
pub struct TaskResult {
    pub task_id: String,
    pub domain: String,
    pub k: u32,
    pub seed: Option<u64>,
    pub status: String,
    pub modes: BTreeMap<String, serde_json::Value>,
    pub selections: BTreeMap<String, serde_json::Value>,
    pub base: Vec<serde_json::Value>,
    pub errors: Vec<String>,
}

impl TaskResult {
    fn new(task_id: &str, domain: &str, k: u32, seed: Option<u64>) -> Self {
        Self {
            task_id: task_id.into(),
            domain: domain.into(),
            k,
            seed,
            status: "running".into(),
            modes: BTreeMap::new(),
            selections: BTreeMap::new(),
            base: Vec::new(),
            errors: Vec::new(),
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
}

/// Options for one `run_task` invocation.
pub struct TaskOptions {
    pub modes: Vec<String>,
    pub seed: Option<u64>,
    pub tag: String,
    pub libraries: BTreeMap<String, Library>,
    pub pool_policy: String,
}

impl Default for TaskOptions {
    fn default() -> Self {
        Self {
            modes: MODES.iter().map(|s| s.to_string()).collect(),
            seed: None,
            tag: String::new(),
            libraries: BTreeMap::new(),
            pool_policy: "finals".into(),
        }
    }
}

/// Roll the pending (label, run_id) reference executions.
async fn roll_bases(
    adapter: &Arc<dyn Adapter>,
    pending: Vec<(String, String)>,
    dead_dir: &Path,
) -> BTreeMap<String, RunView> {
    let mut rolled: BTreeMap<String, RunView> = BTreeMap::new();
    let mut handles = Vec::new();
    for (label, rid) in pending {
        let adapter = adapter.clone();
        let dead_dir = dead_dir.to_path_buf();
        handles.push(tokio::spawn(async move {
            match adapter.execute("", None, &rid, None).await {
                Ok(rd) => {
                    let exists = rd.exists();
                    let delivered = exists && adapter.delivered(&rd);
                    RunView {
                        label: label.clone(),
                        run_id: rid,
                        run_dir: rd,
                        ok: exists,
                        delivered,
                        cost_usd: 0.0,
                        wall_s: 0.0,
                        error: String::new(),
                    }
                }
                Err(exc) => RunView::dead(&label, &rid, dead_dir.to_path_buf(), &format!("{exc}")),
            }
        }));
    }
    for handle in handles {
        if let Ok(rv) = handle.await {
            rolled.insert(rv.label.clone(), rv);
        }
    }
    rolled
}

/// Replay a skipped task into its libraries from the saved chosen harness.
fn restore_libraries(
    surface: &super::tree::SurfaceSpec,
    task_id: &str,
    k: u32,
    ns_base: (&str, &str),
    runs_root_dir: &Path,
    modes: &[String],
    seed: Option<u64>,
    tag: &str,
    libraries: &BTreeMap<String, Library>,
    selections: &BTreeMap<String, serde_json::Value>,
) {
    for mode in modes {
        let Some(lib) = libraries.get(mode) else {
            continue;
        };
        let Some(sel) = selections.get(mode) else {
            continue;
        };
        let label = sel
            .get("chosen_label")
            .and_then(|l| l.as_str())
            .unwrap_or("");
        let ok = sel.get("ok").and_then(|b| b.as_bool()).unwrap_or(false);
        let has_experience = sel
            .get("experience")
            .map(|e| {
                ["cue", "lesson", "evidence"].iter().all(|key| {
                    e.get(key)
                        .and_then(|v| v.as_str())
                        .map(|s| !s.trim().is_empty())
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);
        if !ok || label.is_empty() || !has_experience {
            continue;
        }
        let (bench, sub) = ns_base;
        let n = Namespace::new(bench, sub, mode, k, seed, tag);
        let harness_dir = n
            .task_dir(runs_root_dir, task_id)
            .join("harnesses")
            .join(label);
        let harness = tree::read_tree(surface, &harness_dir);
        if harness.is_empty() {
            continue;
        }
        let experience: Option<library::Experience> = sel
            .get("experience")
            .and_then(|e| serde_json::from_value(e.clone()).ok());
        let _ = lib.add(
            surface,
            task_id,
            &harness,
            sel.get("why").and_then(|w| w.as_str()).unwrap_or(""),
            label,
            sel.get("chosen_run_id")
                .and_then(|r| r.as_str())
                .unwrap_or(""),
            experience.as_ref(),
        );
    }
}

/// Merge `res` into the on-disk summary (accumulating per-mode records), then
/// derive the merged status.
fn write_summary(summary_path: &Path, res: &TaskResult) {
    let mut merged = res.to_json();
    if summary_path.is_file() {
        if let Ok(old_bytes) = std::fs::read(summary_path) {
            if let Ok(old) = serde_json::from_slice::<serde_json::Value>(&old_bytes) {
                for key in ["modes", "selections"] {
                    let mut acc = old
                        .get(key)
                        .and_then(|v| v.as_object())
                        .cloned()
                        .unwrap_or_default();
                    if let Some(new) = merged.get(key).and_then(|v| v.as_object()).cloned() {
                        for (k, v) in new {
                            acc.insert(k, v);
                        }
                    }
                    merged[key] = serde_json::Value::Object(acc);
                }
                let mut errors: Vec<serde_json::Value> = old
                    .get("errors")
                    .and_then(|e| e.as_array())
                    .cloned()
                    .unwrap_or_default();
                if let Some(new_errs) = merged.get("errors").and_then(|e| e.as_array()).cloned() {
                    for e in new_errs {
                        if !errors.contains(&e) {
                            errors.push(e);
                        }
                    }
                }
                merged["errors"] = serde_json::Value::Array(errors);
            }
        }
    }
    // Derived status: ok iff every requested mode has ok mode + selection.
    if res.status == "partial" || res.status == "ok" {
        let modes = merged.get("modes").and_then(|m| m.as_object());
        let sels = merged.get("selections").and_then(|m| m.as_object());
        let all_ok = modes
            .zip(sels)
            .map(|(modes, sels)| {
                !modes.is_empty()
                    && merged
                        .get("errors")
                        .and_then(|e| e.as_array())
                        .map(|e| e.is_empty())
                        .unwrap_or(true)
                    && modes.iter().all(|(mode, value)| {
                        value.get("ok").and_then(|b| b.as_bool()).unwrap_or(false)
                            && sels
                                .get(mode)
                                .map(|s| {
                                    s.get("ok").and_then(|b| b.as_bool()).unwrap_or(false)
                                        && s.get("chosen_label")
                                            .and_then(|l| l.as_str())
                                            .map(|l| !l.is_empty())
                                            .unwrap_or(false)
                                })
                                .unwrap_or(false)
                    })
            })
            .unwrap_or(false);
        merged["status"] = if all_ok {
            serde_json::Value::String("ok".into())
        } else {
            serde_json::Value::String("partial".into())
        };
    }
    let tmp = summary_path.with_extension("json.tmp");
    if std::fs::write(&tmp, serde_json::to_vec_pretty(&merged).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, summary_path);
    }
}

/// Fill `res` from a cached summary covering every requested mode; true if it
/// did. The adopted artifacts are re-verified before reuse.
fn resume(
    summary_path: &Path,
    task_id: &str,
    k: u32,
    surface: &super::tree::SurfaceSpec,
    adapter: &Arc<dyn Adapter>,
    runs_root_dir: &Path,
    modes: &[String],
    seed: Option<u64>,
    tag: &str,
    ns_base: (&str, &str),
    mode_libs: &BTreeMap<String, Library>,
    res: &mut TaskResult,
) -> bool {
    let Ok(bytes) = std::fs::read(summary_path) else {
        return false;
    };
    let Ok(got) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        eprintln!("  [{task_id}] unreadable summary.json; re-running");
        return false;
    };
    if got.get("status").and_then(|s| s.as_str()) != Some("ok") {
        return false;
    }
    let cached_modes = got
        .get("modes")
        .and_then(|m| m.as_object())
        .cloned()
        .unwrap_or_default();
    let cached_sels = got
        .get("selections")
        .and_then(|m| m.as_object())
        .cloned()
        .unwrap_or_default();
    for mode in modes {
        let complete = cached_modes
            .get(mode)
            .map(|m| m.get("ok").and_then(|b| b.as_bool()).unwrap_or(false))
            .unwrap_or(false)
            && cached_sels
                .get(mode)
                .map(|s| {
                    s.get("ok").and_then(|b| b.as_bool()).unwrap_or(false)
                        && s.get("chosen_label")
                            .and_then(|l| l.as_str())
                            .map(|l| !l.is_empty())
                            .unwrap_or(false)
                })
                .unwrap_or(false);
        if !complete {
            return false;
        }
        let sel = &cached_sels[mode];
        let (bench, sub) = ns_base;
        let namespace = Namespace::new(bench, sub, mode, k, seed, tag);
        let label = sel
            .get("chosen_label")
            .and_then(|l| l.as_str())
            .unwrap_or("");
        let harness_dir = namespace
            .task_dir(runs_root_dir, task_id)
            .join("harnesses")
            .join(label);
        let run_dir = PathBuf::from(
            sel.get("chosen_run_dir")
                .and_then(|r| r.as_str())
                .unwrap_or(""),
        );
        let artifact = if sel.get("evidence").and_then(|e| e.as_str()) == Some("trajectory") {
            adapter
                .trajectory(&run_dir, adapter.spec().obs_trunc)
                .unwrap_or_default()
        } else if adapter.delivered(&run_dir) {
            adapter.deliverable_text(&run_dir).unwrap_or_default()
        } else {
            String::new()
        };
        if artifact.trim().is_empty() || artifact.starts_with("(no trajectory") {
            eprintln!("  [{task_id}] cannot resume {mode}: adopted output is missing; re-running");
            return false;
        }
        if !tree::is_tree(surface, &harness_dir) {
            eprintln!("  [{task_id}] cannot resume {mode}: adopted harness is missing; re-running");
            return false;
        }
        if let Some(fp) = sel.get("harness_fingerprint").and_then(|f| f.as_str()) {
            if runmark::fingerprint(Some(&harness_dir)) != fp {
                eprintln!(
                    "  [{task_id}] cannot resume {mode}: adopted harness has changed; re-running"
                );
                return false;
            }
        }
    }
    res.status = "skipped".into();
    if let Some(b) = got.get("base").and_then(|b| b.as_array()).cloned() {
        res.base = b;
    }
    res.modes = cached_modes.into_iter().collect();
    res.selections = cached_sels.into_iter().collect();
    if let Some(e) = got.get("errors").and_then(|e| e.as_array()).cloned() {
        res.errors = e
            .into_iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect();
    }
    restore_libraries(
        surface,
        task_id,
        k,
        ns_base,
        runs_root_dir,
        modes,
        seed,
        tag,
        mode_libs,
        &res.selections,
    );
    true
}

/// Run one recursion mode over the shared bases, select a final, and record
/// it on `res`.
#[allow(clippy::too_many_arguments)]
async fn run_one_mode(
    mode: &str,
    task_id: &str,
    k: u32,
    base: &[RunView],
    adapter: Arc<dyn Adapter>,
    prop: Arc<super::proposer::Proposer>,
    selector: Arc<super::proposer::Proposer>,
    n: &Namespace,
    runs_root_dir: &Path,
    mode_lib: Option<&Library>,
    base_tree: &Tree,
    surface: &super::tree::SurfaceSpec,
    pool_policy: &str,
    res: &mut TaskResult,
) {
    let work_dir = n.task_dir(runs_root_dir, task_id);
    let lib_dir = mode_lib.and_then(|l| l.context_dir());
    let ns_for_ids = n.clone();
    let mode_result = recursion::run_mode(
        mode,
        task_id,
        k,
        base,
        adapter.clone(),
        prop,
        &work_dir,
        |label| ns_for_ids.run_id(task_id, label),
        base_tree,
        lib_dir.as_deref(),
    )
    .await;
    let mode_result = match mode_result {
        Ok(r) => r,
        Err(exc) => {
            res.modes.insert(
                mode.to_string(),
                serde_json::json!({"ok": false, "error": exc.to_string()}),
            );
            res.errors.push(format!("{mode}: {exc}"));
            return;
        }
    };
    res.modes.insert(mode.to_string(), mode_result.to_json());
    let candidates = selection::selection_pool(mode, k, &mode_result.rollouts, base);
    let mut sel = if pool_policy == "finals" && k == 1 {
        selection::trivial_selection(task_id, k, mode, &candidates, adapter.as_ref(), &work_dir)
    } else {
        selection::run_selection(
            task_id,
            k,
            mode,
            &candidates,
            adapter.as_ref(),
            &selector,
            &work_dir,
            "finals",
        )
        .await
    };
    if let Some(lib) = mode_lib {
        if sel.ok {
            if let Some(experience) = selection::reflect_adoption(
                task_id,
                &mut sel,
                &mode_result.rollouts,
                adapter.as_ref(),
                &selector,
                &work_dir,
                &mode_result
                    .stages
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
                    .collect(),
            )
            .await
            {
                let chosen_tree = mode_result
                    .trees
                    .get(&sel.chosen_label)
                    .cloned()
                    .unwrap_or_else(|| base_tree.clone());
                let _ = lib.add(
                    surface,
                    task_id,
                    &chosen_tree,
                    &sel.why,
                    &sel.chosen_label,
                    &sel.chosen_run_id,
                    Some(&experience),
                );
            }
        }
    }
    let mut record = sel.to_json();
    record["chosen_run_dir"] = serde_json::Value::String(
        candidates
            .get(&sel.chosen_label)
            .map(|rv| rv.run_dir.to_string_lossy().into_owned())
            .unwrap_or_default(),
    );
    let chosen_tree = mode_result
        .trees
        .get(&sel.chosen_label)
        .cloned()
        .unwrap_or_else(|| base_tree.clone());
    record["chosen_tree_is_base"] =
        serde_json::Value::Bool(tree_is_base(surface, base_tree, &chosen_tree));
    if !sel.chosen_label.is_empty() {
        record["harness_fingerprint"] = serde_json::Value::String(runmark::fingerprint(Some(
            &work_dir.join("harnesses").join(&sel.chosen_label),
        )));
    }
    res.selections.insert(mode.to_string(), record);
    if !sel.ok {
        res.errors
            .push(format!("{mode}: selection failed: {}", sel.error));
    }
    if let Some(chosen) = candidates.get(&sel.chosen_label) {
        if sel.ok && !chosen.delivered {
            res.errors.push(format!(
                "{mode}: selected run {} produced no deliverable",
                chosen.run_id
            ));
        }
    }
}

/// k shared reference executions -> every mode -> blind selection per mode ->
/// summary.json.
pub async fn run_task(
    adapter: Arc<dyn Adapter>,
    prop: Arc<super::proposer::Proposer>,
    runs_root_dir: &Path,
    task_id: &str,
    k: u32,
    options: TaskOptions,
) -> anyhow::Result<TaskResult> {
    if k < 1 {
        anyhow::bail!("k must be positive, got {k}");
    }
    if options.pool_policy != "finals" {
        anyhow::bail!(
            "unknown pool_policy {:?}; this release builds exactly one selection pool, \
             'finals' (the k composed finals)",
            options.pool_policy
        );
    }
    let mut modes = options.modes.clone();
    if modes.is_empty() {
        anyhow::bail!("modes must contain at least one distinct recursion mode");
    }
    {
        let mut seen = std::collections::BTreeSet::new();
        for m in &modes {
            if !MODES.contains(&m.as_str()) {
                anyhow::bail!("unknown mode {m:?}; expected a subset of {MODES:?}");
            }
            if !seen.insert(m.clone()) {
                anyhow::bail!("modes must contain at least one distinct recursion mode");
            }
        }
    }
    modes.sort();
    let bench = adapter.spec().name.clone();
    let sub = super::substrate::namespace_token(adapter.substrate_name()).to_string();
    let domain = adapter.domain_of(task_id);
    let mut res = TaskResult::new(task_id, &domain, k, options.seed);
    let base_ns = Namespace::new(&bench, &sub, "agg", k, options.seed, &options.tag);
    let work = base_ns.task_dir(runs_root_dir, task_id);
    std::fs::create_dir_all(&work)?;
    let summary_path = work.join("summary.json");

    let surface =
        super::substrate::surface_by_name(adapter.substrate_name()).map_err(anyhow::Error::msg)?;
    if summary_path.is_file()
        && resume(
            &summary_path,
            task_id,
            k,
            &surface,
            &adapter,
            runs_root_dir,
            &modes,
            options.seed,
            &options.tag,
            (&bench, &sub),
            &options.libraries,
            &mut res,
        )
    {
        return Ok(res);
    }

    let base_tree = tree::read_tree(&surface, &adapter.base_harness());

    let labels: Vec<String> = (1..=k as usize).map(|i| format!("base{i}")).collect();
    let pending: Vec<(String, String)> = labels
        .iter()
        .map(|label| (label.clone(), base_ns.run_id(task_id, label)))
        .collect();
    let dead_dir = work.join("missing");
    let mut rolled = roll_bases(&adapter, pending, &dead_dir).await;
    // The adapter may need the task id per rollout; re-roll serially when a
    // concurrent empty-task pass produced nothing.
    if rolled.values().all(|rv| !rv.ok) {
        rolled.clear();
        for (label, rid) in labels
            .iter()
            .map(|l| (l.clone(), base_ns.run_id(task_id, l)))
        {
            let task_owned = task_id.to_string();
            match adapter.execute(&task_owned, None, &rid, None).await {
                Ok(rd) => {
                    let exists = rd.exists();
                    let delivered = exists && adapter.delivered(&rd);
                    rolled.insert(
                        label.clone(),
                        RunView {
                            label,
                            run_id: rid,
                            run_dir: rd,
                            ok: exists,
                            delivered,
                            cost_usd: 0.0,
                            wall_s: 0.0,
                            error: String::new(),
                        },
                    );
                }
                Err(exc) => {
                    rolled.insert(
                        label.clone(),
                        RunView::dead(&label, &rid, dead_dir.clone(), &format!("{exc}")),
                    );
                }
            }
        }
    }
    let base: Vec<RunView> = labels
        .iter()
        .map(|label| {
            rolled.remove(label).unwrap_or_else(|| {
                RunView::dead(
                    label,
                    &base_ns.run_id(task_id, label),
                    dead_dir.clone(),
                    "missing",
                )
            })
        })
        .collect();
    res.base = base
        .iter()
        .map(|b| {
            serde_json::json!({
                "label": b.label,
                "run_id": b.run_id,
                "run_dir": b.run_dir.to_string_lossy(),
            })
        })
        .collect();
    if !base.iter().any(|b| b.ok) {
        res.status = "base-failed".into();
        res.errors.push("no base rollout produced anything".into());
        write_summary(&summary_path, &res);
        return Ok(res);
    }

    for mode in &modes {
        let n = Namespace::new(&bench, &sub, mode, k, options.seed, &options.tag);
        run_one_mode(
            mode,
            task_id,
            k,
            &base,
            adapter.clone(),
            prop.clone(),
            prop.clone(),
            &n,
            runs_root_dir,
            options.libraries.get(mode),
            &base_tree,
            &surface,
            &options.pool_policy,
            &mut res,
        )
        .await;
        if res.status != "skipped" {
            res.status = "partial".into();
        }
        write_summary(&summary_path, &res);
    }

    // Derive the final status.
    let all_ok = !res.modes.is_empty()
        && res.errors.is_empty()
        && modes.iter().all(|mode| {
            res.modes
                .get(mode)
                .map(|m| m.get("ok").and_then(|b| b.as_bool()).unwrap_or(false))
                .unwrap_or(false)
                && res
                    .selections
                    .get(mode)
                    .map(|s| {
                        s.get("ok").and_then(|b| b.as_bool()).unwrap_or(false)
                            && s.get("chosen_label")
                                .and_then(|l| l.as_str())
                                .map(|l| !l.is_empty())
                                .unwrap_or(false)
                    })
                    .unwrap_or(false)
        });
    res.status = if all_ok {
        "ok".into()
    } else {
        "partial".into()
    };
    write_summary(&summary_path, &res);
    Ok(res)
}

/// Every task in one domain, in stream order, threading the library forward.
pub async fn run_domain(
    adapter: Arc<dyn Adapter>,
    prop: Arc<super::proposer::Proposer>,
    runs_root_dir: &Path,
    library_root: &Path,
    domain: &str,
    task_ids: &[String],
    k: u32,
    seed: Option<u64>,
    tag: &str,
    modes: &[String],
) -> serde_json::Value {
    let surface =
        super::substrate::surface_by_name(adapter.substrate_name()).expect("known substrate");
    let libs: BTreeMap<String, Library> = modes
        .iter()
        .map(|mode| {
            (
                mode.clone(),
                Library::new(library_root.join(mode), domain, surface.name),
            )
        })
        .collect();
    let mut done = 0usize;
    let mut failed = 0usize;
    let mut rows = Vec::new();
    for (i, tid) in task_ids.iter().enumerate() {
        for lib in libs.values() {
            lib.retain(&task_ids[..i]);
        }
        let before: BTreeMap<String, usize> = libs
            .iter()
            .map(|(m, lib)| (m.clone(), lib.entries().len()))
            .collect();
        let options = TaskOptions {
            modes: modes.to_vec(),
            seed,
            tag: tag.to_string(),
            libraries: libs.clone(),
            pool_policy: "finals".into(),
        };
        let outcome = run_task(
            adapter.clone(),
            prop.clone(),
            runs_root_dir,
            tid,
            k,
            options,
        )
        .await;
        let row = match outcome {
            Ok(r) => {
                if r.status == "ok" || r.status == "skipped" {
                    done += 1;
                    serde_json::json!({
                        "task": tid,
                        "task_id": tid,
                        "status": r.status,
                        "library_entries_before": before,
                        "selections": r.selections,
                    })
                } else {
                    failed += 1;
                    serde_json::json!({
                        "task": tid,
                        "task_id": tid,
                        "status": r.status,
                        "error": if r.errors.is_empty() { r.status.clone() } else { r.errors.join("; ") },
                        "library_entries_before": before,
                        "selections": r.selections,
                    })
                }
            }
            Err(exc) => {
                failed += 1;
                serde_json::json!({"task": tid, "task_id": tid, "status": "exception", "error": exc.to_string()})
            }
        };
        let err_suffix = row
            .get("error")
            .and_then(|e| e.as_str())
            .map(|e| format!("  :: {e}"))
            .unwrap_or_default();
        eprintln!(
            "  [{domain}] {}/{} {:<12} {}{}",
            i + 1,
            task_ids.len(),
            row.get("status").and_then(|s| s.as_str()).unwrap_or("?"),
            tid,
            err_suffix
        );
        rows.push(row);
    }
    serde_json::json!({
        "domain": domain,
        "n_tasks": task_ids.len(),
        "seed": seed,
        "done": done,
        "failed": failed,
        "tasks": rows,
    })
}

/// Resolve the ordered task stream for execution, previews, and resume
/// identity.
pub fn task_stream(
    adapter: &dyn Adapter,
    split: &str,
    seed: Option<u64>,
    only_domains: &[String],
    only_tasks: &[String],
    max_tasks: Option<usize>,
) -> Result<BTreeMap<String, Vec<String>>, String> {
    let mut by_domain: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for t in adapter.tasks_for_split(split) {
        by_domain.entry(adapter.domain_of(&t)).or_default().push(t);
    }
    if !only_domains.is_empty() {
        let keep: std::collections::BTreeSet<&String> = only_domains.iter().collect();
        let missing: Vec<String> = only_domains
            .iter()
            .filter(|d| !by_domain.contains_key(d.as_str()))
            .cloned()
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "only_domains names unknown domain(s) {missing:?}; split {split:?} has {:?}",
                by_domain.keys().collect::<Vec<_>>()
            ));
        }
        by_domain.retain(|d, _| keep.contains(&d));
    }
    if !only_tasks.is_empty() {
        let wanted: std::collections::BTreeSet<&String> = only_tasks.iter().collect();
        let known: std::collections::BTreeSet<&String> = by_domain.values().flatten().collect();
        let missing: Vec<String> = wanted
            .iter()
            .filter(|t| !known.contains(*t))
            .map(|t| t.to_string())
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "only_tasks names unknown task id(s) {missing:?} for split {split:?}"
            ));
        }
    }
    if max_tasks.map(|m| m < 1).unwrap_or(false) {
        return Err(format!("max_tasks must be positive, got {:?}", max_tasks));
    }
    let reference = default_reference_domain_order(&by_domain);
    let stream = stream_order(&by_domain, seed, &reference)?;
    let mut ordered: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (d, t) in stream {
        if !only_tasks.is_empty() && !only_tasks.iter().any(|x| x == &t) {
            continue;
        }
        if let Some(max) = max_tasks {
            if ordered.entry(d.clone()).or_default().len() >= max {
                continue;
            }
        }
        ordered.entry(d).or_default().push(t);
    }
    Ok(ordered)
}

/// Run domains sequentially in stream order (the desktop runs one domain at a
/// time; sweeps across domains are a CLI concern and stay serial here so a
/// library never races itself).
#[allow(clippy::too_many_arguments)]
pub async fn run_sweep(
    adapter: Arc<dyn Adapter>,
    prop: Arc<super::proposer::Proposer>,
    runs_root_dir: &Path,
    library_root: &Path,
    k: u32,
    split: &str,
    seed: Option<u64>,
    tag: &str,
    modes: &[String],
    only_domains: &[String],
    only_tasks: &[String],
    max_tasks: Option<usize>,
) -> anyhow::Result<serde_json::Value> {
    let ordered_tasks = task_stream(
        adapter.as_ref(),
        split,
        seed,
        only_domains,
        only_tasks,
        max_tasks,
    )
    .map_err(anyhow::Error::msg)?;
    let order: Vec<String> = ordered_tasks.keys().cloned().collect();
    let started = chrono::Utc::now().to_rfc3339();
    let mut rows = Vec::new();
    for d in &order {
        let tasks = &ordered_tasks[d];
        rows.push(
            run_domain(
                adapter.clone(),
                prop.clone(),
                runs_root_dir,
                library_root,
                d,
                tasks,
                k,
                seed,
                tag,
                modes,
            )
            .await,
        );
    }
    Ok(serde_json::json!({
        "benchmark": adapter.spec().name,
        "substrate": adapter.substrate_name(),
        "k": k,
        "modes": modes,
        "seed": seed,
        "tag": tag,
        "domain_order": order,
        "max_tasks": max_tasks,
        "only_tasks": only_tasks,
        "started_at": started,
        "finished_at": chrono::Utc::now().to_rfc3339(),
        "n_domains": order.len(),
        "domains": rows,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespaces_and_tags() {
        assert_eq!(
            ns("demo", "oc", "agg", 2, None, "").unwrap(),
            "demo-oc-agg2"
        );
        assert_eq!(
            ns("demo", "oc", "seq", 1, Some(7), "t1").unwrap(),
            "demo-oc-seq1-s7-t1"
        );
        assert!(ns("demo", "oc", "nope", 1, None, "").is_err());
        assert!(ns("demo", "", "agg", 1, None, "").is_err());
        assert!(validate_tag("ok-tag_1").is_ok());
        assert!(validate_tag("-lead").is_err());
        assert!(validate_tag("s12").is_err());
        assert!(validate_tag("a/b").is_err());
    }

    #[test]
    fn stream_order_is_seed_deterministic_and_subset_stable() {
        let mut by_domain = BTreeMap::new();
        by_domain.insert("a".to_string(), vec!["a2".to_string(), "a1".to_string()]);
        by_domain.insert("b".to_string(), vec!["b1".to_string()]);
        let s7 = stream_order(&by_domain, Some(7), &[]).unwrap();
        let s7b = stream_order(&by_domain, Some(7), &[]).unwrap();
        assert_eq!(s7, s7b);
        let reference = vec!["a".to_string(), "b".to_string()];
        let none = stream_order(&by_domain, None, &reference).unwrap();
        assert_eq!(
            none,
            vec![
                ("a".to_string(), "a1".to_string()),
                ("a".to_string(), "a2".to_string()),
                ("b".to_string(), "b1".to_string()),
            ]
        );
        // Dropping one domain leaves the other's stream identical.
        let mut by_domain2 = by_domain.clone();
        by_domain2.remove("b");
        let s7c = stream_order(&by_domain2, Some(7), &[]).unwrap();
        let a_stream: Vec<_> = s7.iter().filter(|(d, _)| d == "a").collect();
        let a_stream2: Vec<_> = s7c.iter().collect();
        assert_eq!(a_stream, a_stream2);
    }

    #[test]
    fn unknown_reference_domain_is_an_error() {
        let mut by_domain = BTreeMap::new();
        by_domain.insert("z".to_string(), vec!["t".to_string()]);
        assert!(stream_order(&by_domain, None, &["other".to_string()]).is_err());
    }
}
