//! Choose an existing anonymized final output, then reflect on its
//! refinement evidence.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::SeededRandom;
use super::adapter::{Adapter, RunView};
use super::context::{self, PoolEntry};
use super::library::Experience;
use super::prompts as P;
use super::proposer::{Proposer, ProposerError};

#[derive(Debug, Clone, Serialize)]
pub struct Selection {
    pub task_id: String,
    pub mode: String,
    pub k: u32,
    pub chosen_id: String,
    pub chosen_label: String,
    pub chosen_run_id: String,
    pub chosen_is_base: bool,
    pub why: String,
    pub id_map: BTreeMap<String, String>,
    pub dropped: Vec<String>,
    pub n_candidates: usize,
    pub ok: bool,
    pub fallback: bool,
    pub error: String,
    pub usd: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub experience: Option<Experience>,
    #[serde(skip)]
    pub trivial: bool,
    pub evidence: String,
}

impl Selection {
    fn new(task_id: &str, mode: &str, k: u32) -> Self {
        Self {
            task_id: task_id.into(),
            mode: mode.into(),
            k,
            chosen_id: String::new(),
            chosen_label: String::new(),
            chosen_run_id: String::new(),
            chosen_is_base: false,
            why: String::new(),
            id_map: BTreeMap::new(),
            dropped: Vec::new(),
            n_candidates: 0,
            ok: false,
            fallback: false,
            error: String::new(),
            usd: 0.0,
            experience: None,
            trivial: false,
            evidence: "deliverable".into(),
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
}

/// Graded items first, in their original relative order; nothing is discarded.
pub fn order_graded_first<T>(mut items: Vec<T>, graded: impl Fn(&T) -> bool) -> Vec<T> {
    let mut head = Vec::new();
    let mut tail = Vec::new();
    for item in items.drain(..) {
        if graded(&item) {
            head.push(item);
        } else {
            tail.push(item);
        }
    }
    head.extend(tail);
    head
}

fn is_base(label: &str) -> bool {
    label.starts_with("base")
}

/// anon id -> label, by canonical sort then a seeded shuffle: stable across
/// reruns.
pub fn assign_ids(
    labels: &[String],
    task_id: &str,
    k: u32,
    mode: &str,
) -> BTreeMap<String, String> {
    let mut ordered = labels.to_vec();
    ordered.sort();
    let mut rng = SeededRandom::new(&format!("{task_id}|{k}|{mode}"));
    rng.shuffle(&mut ordered);
    ordered
        .into_iter()
        .enumerate()
        .map(|(i, lbl)| (anon_id(i), lbl))
        .collect()
}

/// A..Z, AA..AZ, BA... -- always accepted by the choice parser.
fn anon_id(mut index: usize) -> String {
    let mut out = String::new();
    let mut n = index + 1;
    while n > 0 {
        let rem = (n - 1) % 26;
        n = (n - 1) / 26;
        out.insert(0, (b'A' + rem as u8) as char);
        index += 0; // no-op keeps `index` used for signature parity
    }
    out
}

/// The one home for selection-pool construction: exactly the k finals of
/// `mode`. Labels derive from k -- agg1..aggk for the parallel mode, c1..ck
/// for the sequential one -- so the reference executions are not in the pool
/// and there is no abstention by construction. `base` is accepted and
/// deliberately unused: it is what the pool excludes.
pub fn selection_pool(
    mode: &str,
    k: u32,
    rollouts: &BTreeMap<String, RunView>,
    base: &[RunView],
) -> BTreeMap<String, RunView> {
    let _ = base;
    let labels: Vec<String> = (1..=k as usize)
        .map(|i| {
            if mode == "agg" {
                format!("agg{i}")
            } else {
                format!("c{i}")
            }
        })
        .collect();
    labels
        .into_iter()
        .filter(|lbl| rollouts.contains_key(lbl))
        .filter_map(|lbl| rollouts.get(&lbl).cloned().map(|rv| (lbl, rv)))
        .collect()
}

fn render_pool(
    candidates: &BTreeMap<String, RunView>,
    adapter: &dyn Adapter,
    sel: &mut Selection,
) -> (BTreeMap<String, RunView>, BTreeMap<String, String>) {
    let mut pool = BTreeMap::new();
    let mut artifacts = BTreeMap::new();
    for (lbl, rv) in candidates {
        if !(rv.ok && rv.delivered) {
            sel.dropped.push(lbl.clone());
            continue;
        }
        match adapter.deliverable_text(&rv.run_dir) {
            Ok(rendered) if !rendered.trim().is_empty() => {
                pool.insert(lbl.clone(), rv.clone());
                artifacts.insert(lbl.clone(), rendered);
            }
            Ok(_) => sel.dropped.push(lbl.clone()),
            Err(exc) => {
                let detail = format!("{lbl} (render failed: {exc})");
                sel.dropped.push(detail.chars().take(200).collect());
            }
        }
    }
    if pool.is_empty() && !candidates.is_empty() && adapter.spec().state_deliverable {
        for (lbl, rv) in candidates {
            if !rv.ok {
                continue;
            }
            let Ok(body) = adapter.trajectory(&rv.run_dir, adapter.spec().obs_trunc) else {
                continue;
            };
            if body.trim().is_empty() || body.starts_with("(no trajectory") {
                continue;
            }
            pool.insert(lbl.clone(), rv.clone());
            artifacts.insert(
                lbl.clone(),
                format!(
                    "(this task produced no file deliverable; the candidate is shown as its \
                     own session transcript)\n\n{body}"
                ),
            );
        }
        if !pool.is_empty() {
            sel.evidence = "trajectory".into();
            sel.dropped.retain(|d| !pool.contains_key(d));
        }
    }
    (pool, artifacts)
}

fn write_candidates(
    sel: &Selection,
    artifacts: &BTreeMap<String, String>,
    pool: &BTreeMap<String, RunView>,
    cand_dir: &Path,
) -> std::io::Result<()> {
    std::fs::create_dir_all(cand_dir)?;
    for (anon, lbl) in &sel.id_map {
        let Some(text) = artifacts.get(lbl) else {
            continue;
        };
        let lines = text.matches('\n').count() + 1;
        let header = format!(
            "<!-- candidate {anon}: {} chars, {lines} lines. COMPLETE -- nothing trimmed. \
             Navigate with grep/offsets; evaluating a candidate means opening the part that \
             determines whether it works.\n-->\n",
            text.len()
        );
        std::fs::write(
            cand_dir.join(format!("{anon}.md")),
            format!("{header}{text}"),
        )?;
        let _ = pool; // asset copying (rendered evidence) is adapter-optional in this port
    }
    Ok(())
}

fn fallback(
    sel: &mut Selection,
    pool: &BTreeMap<String, RunView>,
    pool_kind: &str,
    exc: &str,
) -> bool {
    let base: Vec<String> = pool.keys().filter(|l| is_base(l)).cloned().collect();
    if let Some(first_base) = base.first() {
        sel.chosen_label = first_base.clone();
    } else if pool_kind == "finals" {
        match pool.keys().next() {
            Some(first) => sel.chosen_label = first.clone(),
            None => {
                sel.error = format!(
                    "{exc}; no delivered base artifact was available for a neutral fallback"
                );
                return false;
            }
        }
    } else {
        sel.error = format!(
            "{}; no delivered base artifact was available for a neutral fallback",
            exc
        );
        return false;
    }
    sel.chosen_id = sel
        .id_map
        .iter()
        .find(|(_, label)| *label == &sel.chosen_label)
        .map(|(anon, _)| anon.clone())
        .unwrap_or_default();
    sel.fallback = true;
    sel.error = exc.chars().take(400).collect();
    true
}

fn persist(sel: &mut Selection, root: &Path, run_dir: &str) {
    let mut rec = sel.to_json();
    rec["chosen_run_dir"] = serde_json::Value::String(run_dir.to_string());
    if let Err(exc) = std::fs::write(
        root.join("selection.json"),
        serde_json::to_vec_pretty(&rec).unwrap_or_default(),
    ) {
        sel.ok = false;
        let detail = format!("{} | could not persist selection: {exc}", sel.error);
        sel.error = detail.chars().take(400).collect();
    }
}

/// Pick one output. A selector failure falls back to a delivered base and
/// never returns `Err` for selection outcomes; adapter bugs do propagate.
#[allow(clippy::too_many_arguments)]
pub async fn run_selection(
    task_id: &str,
    k: u32,
    mode: &str,
    candidates: &BTreeMap<String, RunView>,
    adapter: &dyn Adapter,
    prop: &Proposer,
    work_dir: &Path,
    pool_kind: &str,
) -> Selection {
    let mut sel = Selection::new(task_id, mode, k);
    let bp = &adapter.spec().prompts;
    let (pool, artifacts) = render_pool(candidates, adapter, &mut sel);
    sel.n_candidates = pool.len();
    if pool.is_empty() {
        sel.error = "no candidate produced an artifact".into();
        return sel;
    }
    let labels: Vec<String> = pool.keys().cloned().collect();
    sel.id_map = assign_ids(&labels, task_id, k, mode);

    let root = work_dir.join("selection").join(format!("{mode}{k}"));
    let _ = std::fs::remove_dir_all(&root);
    if std::fs::create_dir_all(&root).is_err() {
        sel.error = "cannot create the selection workspace".into();
        return sel;
    }
    let _ = std::fs::write(
        root.join("task.md"),
        format!("{}\n", adapter.task_prompt(task_id)),
    );
    let _ = context::copy_source(adapter, task_id, &root);
    if let Err(exc) = write_candidates(&sel, &artifacts, &pool, &root.join("candidates")) {
        sel.error = format!("cannot write candidates: {exc}");
        return sel;
    }
    let ids: Vec<String> = sel.id_map.keys().cloned().collect();
    let mission = format!(
        "{}\n\n# YOUR EVIDENCE\n\nIt is on disk. Read it -- do not answer from this message \
         alone.\n\ncontext/task.md                 the instructions\ncontext/source/         \
         task source materials, when supplied\ncontext/candidates/<id>.md      one file per \
         candidate: {}\n\nOpen every candidate before choosing. Choose exactly one of: {}.",
        P::select(bp, pool.len(), pool_kind, "").unwrap_or_default(),
        ids.join(", "),
        ids.join(", ")
    );
    if let Err(exc) = context::assert_no_grader_verdicts(&root) {
        sel.error = format!("{exc}");
        return sel;
    }

    match prop
        .call(
            &mission,
            Some(&root),
            &format!("select.{mode}{k}"),
            "",
            None,
            None,
        )
        .await
    {
        Ok(reply) => {
            sel.usd += reply.cost_usd;
            let choice = regex::Regex::new(r"```choice\s*\n?\s*([A-Za-z0-9_-]+)\s*\n?\s*```")
                .expect("choice regex");
            let Some(m) = choice.captures(&reply.text) else {
                if !fallback(
                    &mut sel,
                    &pool,
                    pool_kind,
                    "no ```choice block in the reply",
                ) {
                    return sel;
                }
                finish_selection(&mut sel, &pool, &root);
                return sel;
            };
            let anon = m[1].trim().to_string();
            if !sel.id_map.contains_key(&anon) {
                let joined = ids.join(", ");
                let msg = format!("chose {anon:?}, which is not one of {joined}");
                if !fallback(&mut sel, &pool, pool_kind, &msg) {
                    return sel;
                }
                finish_selection(&mut sel, &pool, &root);
                return sel;
            }
            let why_re = regex::Regex::new(r"(?s)<why>(.*?)</why>").expect("why regex");
            sel.why = why_re
                .captures(&reply.text)
                .map(|w| w[1].trim().chars().take(2000).collect())
                .unwrap_or_default();
            sel.chosen_id = anon;
            sel.chosen_label = sel.id_map[&sel.chosen_id].clone();
            sel.ok = true;
        }
        Err(exc) => {
            sel.usd += exc.cost_usd;
            if !fallback(&mut sel, &pool, pool_kind, &exc.to_string()) {
                return sel;
            }
        }
    }
    finish_selection(&mut sel, &pool, &root);
    sel
}

fn finish_selection(sel: &mut Selection, pool: &BTreeMap<String, RunView>, root: &Path) {
    if let Some(rv) = pool.get(&sel.chosen_label) {
        sel.chosen_run_id = rv.run_id.clone();
    }
    sel.chosen_is_base = is_base(&sel.chosen_label);
    let run_dir = pool
        .get(&sel.chosen_label)
        .map(|rv| rv.run_dir.to_string_lossy().into_owned())
        .unwrap_or_default();
    persist(sel, root, &run_dir);
}

/// Deliver the single k=1 final without a selector session.
pub fn trivial_selection(
    task_id: &str,
    k: u32,
    mode: &str,
    candidates: &BTreeMap<String, RunView>,
    adapter: &dyn Adapter,
    work_dir: &Path,
) -> Selection {
    let mut sel = Selection::new(task_id, mode, k);
    sel.trivial = true;
    if candidates.len() > 1 {
        sel.error = format!(
            "trivial_selection expects at most one candidate (the k=1 final), got {:?}",
            candidates.keys().collect::<Vec<_>>()
        );
        return sel;
    }
    let (pool, _artifacts) = render_pool(candidates, adapter, &mut sel);
    sel.n_candidates = pool.len();
    if pool.is_empty() {
        sel.error =
            "no candidate produced an artifact (finals-only pool has no base fallback)".into();
        return sel;
    }
    let (label, rv) = pool.iter().next().unwrap();
    sel.ok = true;
    sel.chosen_id = "A".into();
    sel.chosen_label = label.clone();
    sel.id_map.insert("A".into(), label.clone());
    sel.chosen_run_id = rv.run_id.clone();
    sel.chosen_is_base = is_base(label);
    sel.why = "trivial pool: single final at k=1; delivered without a selector session".into();
    let root = work_dir.join("selection").join(format!("{mode}{k}"));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::create_dir_all(&root);
    persist(&mut sel, &root, &rv.run_dir.to_string_lossy());
    sel
}

/// Learn from harness edits, decisions, trajectories, and deliverables after
/// adoption. Returns the experience, or None when reflection found nothing
/// supported.
#[allow(clippy::too_many_arguments)]
pub async fn reflect_adoption(
    task_id: &str,
    sel: &mut Selection,
    candidates: &BTreeMap<String, RunView>,
    adapter: &dyn Adapter,
    prop: &Proposer,
    work_dir: &Path,
    decisions: &BTreeMap<String, String>,
) -> Option<Experience> {
    let chosen = candidates.get(&sel.chosen_label)?.clone();
    if !sel.ok {
        return None;
    }
    let root = work_dir
        .join("reflection")
        .join(format!("{}{}", sel.mode, sel.k));
    let harnesses = work_dir.join("harnesses");
    let pool: Vec<PoolEntry> = candidates
        .iter()
        .filter(|(label, _)| !label.starts_with("base"))
        .map(|(label, rv)| PoolEntry {
            label: label.clone(),
            harness_dir: Some(harnesses.join(label)),
            rollout: Some(rv.clone()),
        })
        .collect();
    let evidence = context::build(
        &root,
        adapter,
        task_id,
        &candidates.values().cloned().collect::<Vec<_>>(),
        Some(&harnesses.join(&sel.chosen_label)),
        None,
        &pool,
        decisions,
        adapter.spec().obs_trunc,
    )
    .ok()?;
    let _ = std::fs::create_dir_all(root.join("base_harness"));
    let _ = super::copy_tree(&adapter.base_harness(), &root.join("base_harness"));
    let adoption = serde_json::json!({
        "chosen_label": sel.chosen_label,
        "candidate_ids": sel.id_map,
        "reason": sel.why,
    });
    let _ = std::fs::write(
        root.join("adoption.json"),
        format!(
            "{}\n",
            serde_json::to_string_pretty(&adoption).unwrap_or_default()
        ),
    );
    let _ = context::assert_no_leak_paths(&root, &adapter.spec().leak_globs);
    let _ = context::assert_no_grader_verdicts(&root);

    // Distil.
    let mission = format!(
        "{}\n\n# WHAT HAPPENED\n\nHarness {} was adopted. The selection reasoning \
         was:\n\n{}\n\ncontext/adoption.json maps any anonymous IDs in that reasoning to \
         rollout labels. Compare context/harness/ with context/base_harness/ and the \
         candidates in context/pool/. Read the decisions and execution contrasts before \
         writing.\n\n{}",
        P::experience(&adapter.spec().prompts),
        sel.chosen_label,
        sel.why,
        evidence.layout()
    );
    let exp_root = root.with_file_name(format!(
        "{}.experience",
        root.file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
    ));
    let _ = std::fs::remove_dir_all(&exp_root);
    let _ = super::copy_tree(&root, &exp_root);
    let _ = std::fs::remove_file(exp_root.join("selection.json"));
    let _ = context::assert_no_grader_verdicts(&exp_root);
    let reply = match prop
        .call(
            &mission,
            Some(&exp_root),
            &format!("experience.{}{}", sel.mode, sel.k),
            "",
            None,
            None,
        )
        .await
    {
        Ok(reply) => {
            sel.usd += reply.cost_usd;
            reply
        }
        Err(exc) => {
            sel.usd += exc.cost_usd;
            return None;
        }
    };
    let edits_re = regex::Regex::new(r"(?s)<edits>(.*?)</edits>").expect("edits regex");
    if let Some(m) = edits_re.captures(&reply.text) {
        let _ = std::fs::write(root.join("edits.md"), format!("{}\n", m[1].trim()));
    }
    let exp_re = regex::Regex::new(r"(?s)<experience>(.*?)</experience>").expect("exp regex");
    let Some(m) = exp_re.captures(&reply.text) else {
        return None;
    };
    let mut cue = String::new();
    let mut lesson = String::new();
    let mut ev = String::new();
    let mut confidence = String::new();
    for line in m[1].trim().lines() {
        if let Some((key, val)) = line.split_once(':') {
            let key = key.trim().to_lowercase();
            let val = val.trim().to_string();
            match key.as_str() {
                "cue" => cue = val,
                "lesson" => lesson = val,
                "evidence" => ev = val,
                "confidence" => confidence = val,
                _ => {}
            }
        }
    }
    if cue.is_empty() || lesson.is_empty() || ev.is_empty() {
        return None;
    }
    let experience = Experience {
        cue,
        lesson,
        evidence: ev,
        confidence,
        edits: edits_re
            .captures(&reply.text)
            .map(|e| e[1].trim().to_string())
            .unwrap_or_default(),
    };
    sel.experience = Some(experience.clone());
    let sel_root = work_dir
        .join("selection")
        .join(format!("{}{}", sel.mode, sel.k));
    let run_dir = chosen.run_dir.to_string_lossy().into_owned();
    persist(sel, &sel_root, &run_dir);
    Some(experience)
}

/// The error type is re-exported for recursion's stage handling.
pub type SelectionError = ProposerError;

#[allow(dead_code)]
fn type_anchor(_p: &PathBuf) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anon_ids_are_alphabetic_and_stable() {
        assert_eq!(anon_id(0), "A");
        assert_eq!(anon_id(25), "Z");
        assert_eq!(anon_id(26), "AA");
        let labels: Vec<String> = ["b", "a", "c"].iter().map(|s| s.to_string()).collect();
        let m1 = assign_ids(&labels, "t", 2, "agg");
        let m2 = assign_ids(&labels, "t", 2, "agg");
        assert_eq!(m1, m2);
        assert_eq!(m1.len(), 3);
    }

    #[test]
    fn pool_is_exactly_the_k_finals() {
        let mut rollouts = BTreeMap::new();
        for label in ["base1", "inv1", "dom1", "agg1", "agg2"] {
            rollouts.insert(
                label.to_string(),
                RunView {
                    label: label.into(),
                    run_id: label.into(),
                    run_dir: PathBuf::from("/tmp"),
                    ok: true,
                    delivered: true,
                    cost_usd: 0.0,
                    wall_s: 0.0,
                    error: String::new(),
                },
            );
        }
        let pool = selection_pool("agg", 2, &rollouts, &[]);
        assert_eq!(pool.len(), 2);
        assert!(pool.contains_key("agg1") && pool.contains_key("agg2"));
        assert!(!pool.contains_key("base1"));
    }

    #[test]
    fn graded_first_preserves_relative_order() {
        let out = order_graded_first(vec![3, 1, 2, 4], |x| x % 2 == 0);
        assert_eq!(out, vec![2, 4, 3, 1]);
    }
}
