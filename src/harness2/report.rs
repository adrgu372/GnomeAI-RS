//! Measurement output: per-task JSON summaries and a readable per-model
//! report. Pure formatting -- reading artifacts, nothing more.

use std::collections::BTreeMap;
use std::path::Path;

use super::judge::{Scored, judge_selections};

pub type Selections = BTreeMap<String, serde_json::Value>;

/// `(passed, total)` pairs for a benchmark's tasks, read from the sweep
/// summaries.
pub fn collect_selections(
    domains: &[serde_json::Value],
    _runs_root: &Path,
) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    for d in domains {
        let Some(tasks) = d.get("tasks").and_then(|t| t.as_array()) else {
            continue;
        };
        for t in tasks {
            if let Some(sels) = t.get("selections").and_then(|s| s.as_object()) {
                for (mode, sel) in sels {
                    let mut rec = sel.clone();
                    rec["task_id"] = t.get("task_id").cloned().unwrap_or_default();
                    rec["domain"] = d.get("domain").cloned().unwrap_or_default();
                    rec["mode"] = serde_json::Value::String(mode.clone());
                    out.push(rec);
                }
            }
        }
        let _ = _runs_root;
    }
    out
}

/// Score every selection record from a finished sweep.
///
/// `score` maps `(task_id, run_dir)` to `(passed, criteria)`; returning `None`
/// means the judge itself failed, which is recorded as an error row and is
/// never counted as a zero. Scoring runs in parallel per rollout.
pub async fn score_sweep(
    sweep: &serde_json::Value,
    score: impl Fn(&str, &Path) -> Option<(i64, i64)> + Send + Sync + Clone + 'static,
    _parallel: usize,
) -> Vec<Scored> {
    let domains = sweep
        .get("domains")
        .and_then(|d| d.as_array())
        .cloned()
        .unwrap_or_default();
    let selections = collect_selections(&domains, Path::new(""));
    judge_selections(selections, move |task_id, run_dir| {
        score(task_id, Path::new(run_dir))
    })
    .await
}

/// The readable report grouped per (mode, k).
pub fn render(
    _sweep: &serde_json::Value,
    rows: &[Scored],
    benchmark: &str,
    substrate: &str,
) -> String {
    let mut by_mode: BTreeMap<(String, u64), Vec<&Scored>> = BTreeMap::new();
    for r in rows {
        by_mode
            .entry((r.mode.clone(), r.k as u64))
            .or_default()
            .push(r);
    }
    let mut lines = Vec::new();
    lines.push(format!("harness2 report -- {benchmark} on {substrate}"));
    lines.push(String::new());
    for ((mode, k), group) in &by_mode {
        let graded: Vec<&&Scored> = group.iter().filter(|r| r.graded()).collect();
        let denom = graded.len();
        let pass = graded.iter().filter(|r| r.all_pass).count();
        let frac_sum: f64 = graded.iter().filter_map(|r| r.frac()).sum();
        let micro = if denom > 0 {
            graded
                .iter()
                .map(|r| r.n_passed.unwrap_or(0) as f64)
                .sum::<f64>()
                / graded
                    .iter()
                    .map(|r| r.n_criteria.unwrap_or(0) as f64)
                    .sum::<f64>()
        } else {
            0.0
        };
        let macro_v = if denom > 0 {
            frac_sum / denom as f64
        } else {
            0.0
        };
        let spend = group.iter().map(|r| r.usd).sum::<f64>();
        lines.push(format!(
            "{mode}{k}: {pass}/{denom} all-pass | pass-rate micro {micro:.3} | macro \
             {macro_v:.3} | {} graded | ${spend:.2}",
            group.len()
        ));
    }
    lines.push(String::new());
    for r in rows {
        let frac = r
            .frac()
            .map(|f| format!("{f:.3}"))
            .unwrap_or_else(|| "--".into());
        lines.push(format!(
            "  {:>12}  {}  {}  frac={frac}  {}",
            r.task_id,
            r.mode,
            r.label,
            if r.error.is_empty() {
                String::new()
            } else {
                format!("error: {}", r.error)
            }
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness2::judge::Scored;

    fn row(task: &str, mode: &str, passed: Option<i64>, criteria: Option<i64>) -> Scored {
        Scored {
            task_id: task.into(),
            mode: mode.into(),
            k: 2,
            label: "agg1".into(),
            run_id: "run".into(),
            chosen_is_base: false,
            n_passed: passed,
            n_criteria: criteria,
            all_pass: matches!((passed, criteria), (Some(p), Some(c)) if p == c && c > 0),
            raw: serde_json::Value::Null,
            error: String::new(),
            usd: 1.0,
        }
    }

    #[test]
    fn ungraded_rows_are_excluded_from_the_denominator() {
        let rows = vec![
            row("t1", "agg", Some(4), Some(4)),
            row("t2", "agg", Some(2), Some(4)),
            // A failed judge is not a zero: it must not depress the pass rate.
            row("t3", "agg", None, None),
        ];
        let text = render(&serde_json::Value::Null, &rows, "demo", "gnomeai");
        assert!(text.contains("1/2 all-pass"), "{text}");
        assert!(text.contains("micro 0.750"), "{text}");
        assert!(text.contains("macro 0.750"), "{text}");
        // The ungraded row still appears, with an unknown fraction.
        assert!(text.contains("frac=--"), "{text}");
    }

    #[test]
    fn selections_are_collected_from_a_sweep() {
        let sweep = serde_json::json!({
            "domains": [{
                "domain": "accounting",
                "tasks": [{
                    "task_id": "ledger-1",
                    "selections": {
                        "agg": {"ok": true, "chosen_label": "agg1", "chosen_run_dir": "/x"}
                    }
                }]
            }]
        });
        let rows = collect_selections(sweep["domains"].as_array().unwrap(), Path::new(""));
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["task_id"], serde_json::json!("ledger-1"));
        assert_eq!(rows[0]["mode"], serde_json::json!("agg"));
        assert_eq!(rows[0]["domain"], serde_json::json!("accounting"));
    }
}
