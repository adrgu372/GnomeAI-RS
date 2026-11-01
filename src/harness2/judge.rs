//! Measurement of the run that selection chose, after it chose it.
//!
//! Nothing in the method imports this module. A judge failure is None, never
//! a zero, and a None is excluded from the denominator.

use serde::Serialize;
use serde_json::Value;

/// One judged selection.
#[derive(Debug, Clone, Serialize)]
pub struct Scored {
    pub task_id: String,
    pub mode: String,
    pub k: u32,
    pub label: String,
    pub run_id: String,
    pub chosen_is_base: bool,
    pub n_passed: Option<i64>,
    pub n_criteria: Option<i64>,
    pub all_pass: bool,
    pub raw: Value,
    pub error: String,
    pub usd: f64,
}

impl Scored {
    /// Whether the judge succeeded, not whether the run did well.
    pub fn graded(&self) -> bool {
        self.n_criteria.map(|n| n > 0).unwrap_or(false)
    }

    pub fn frac(&self) -> Option<f64> {
        match (self.n_passed, self.n_criteria) {
            (Some(p), Some(c)) if c > 0 => Some(p as f64 / c as f64),
            _ => None,
        }
    }
}

/// The mode token of a selection record, reading the legacy key too.
pub fn mode_of(sel: &Value) -> String {
    if let Some(m) = sel.get("mode").and_then(|m| m.as_str()) {
        return m.to_string();
    }
    sel.get("arm")
        .and_then(|m| m.as_str())
        .unwrap_or("")
        .to_string()
}

fn field_str(sel: &Value, key: &str) -> String {
    sel.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// Judge one selection's chosen run. A judge exception is an `error`, never a
/// zero.
pub fn judge_one(score: impl Fn(&str, &str) -> Option<(i64, i64)>, sel: &Value) -> Scored {
    let mut s = Scored {
        task_id: field_str(sel, "task_id"),
        mode: mode_of(sel),
        k: sel.get("k").and_then(|k| k.as_u64()).unwrap_or(0) as u32,
        label: field_str(sel, "chosen_label"),
        run_id: field_str(sel, "chosen_run_id"),
        chosen_is_base: sel
            .get("chosen_is_base")
            .and_then(|b| b.as_bool())
            .unwrap_or(false),
        n_passed: None,
        n_criteria: None,
        all_pass: false,
        raw: Value::Null,
        error: String::new(),
        usd: sel.get("usd").and_then(|u| u.as_f64()).unwrap_or(0.0),
    };
    if !sel.get("ok").and_then(|b| b.as_bool()).unwrap_or(false) || s.label.is_empty() {
        s.error = "selection did not complete successfully".into();
        return s;
    }
    let task = s.task_id.clone();
    let run_dir = field_str(sel, "chosen_run_dir");
    if run_dir.is_empty() {
        s.error = "selection names no run directory".into();
        return s;
    }
    match score(&task, &run_dir) {
        None => {
            s.error = "judge returned nothing".into();
            s
        }
        Some((n_passed, n_criteria)) => {
            s.n_passed = Some(n_passed);
            s.n_criteria = Some(n_criteria);
            s.all_pass = n_criteria > 0 && n_passed == n_criteria;
            s.raw = serde_json::json!({"n_passed": n_passed, "n_criteria": n_criteria});
            s
        }
    }
}

/// Judge each selection record concurrently; failures carry `error` rows.
pub async fn judge_selections(
    selections: Vec<Value>,
    score: impl Fn(&str, &str) -> Option<(i64, i64)> + Send + Sync + Clone + 'static,
) -> Vec<Scored> {
    use futures_util::StreamExt;
    let score = std::sync::Arc::new(score);
    futures_util::stream::iter(selections.into_iter().map(move |sel| {
        let score = score.clone();
        async move {
            // The score closure is sync and quick (artifact reads); keep it
            // off the async worker with a blocking task when it might read
            // large files.
            let handle = tokio::task::spawn_blocking(move || judge_one(|t, r| score(t, r), &sel));
            match handle.await {
                Ok(scored) => scored,
                Err(exc) => Scored {
                    task_id: String::new(),
                    mode: String::new(),
                    k: 0,
                    label: String::new(),
                    run_id: String::new(),
                    chosen_is_base: false,
                    n_passed: None,
                    n_criteria: None,
                    all_pass: false,
                    raw: Value::Null,
                    error: format!("join error: {exc}"),
                    usd: 0.0,
                },
            }
        }
    }))
    .buffer_unordered(8)
    .collect::<Vec<_>>()
    .await
}

/// Write the rows as JSON.
pub fn write(rows: &[Scored], path: &std::path::Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(rows).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_selection_is_never_judged() {
        let sel = serde_json::json!({
            "task_id": "t1", "mode": "agg", "k": 1,
            "ok": false, "chosen_label": "", "usd": 3.0
        });
        let scored = judge_one(|_, _| Some((1, 1)), &sel);
        assert!(!scored.graded());
        assert!(scored.n_passed.is_none());
        assert_eq!(scored.error, "selection did not complete successfully");
    }

    #[test]
    fn legacy_arm_key_is_read() {
        assert_eq!(mode_of(&serde_json::json!({"arm": "seq"})), "seq");
        assert_eq!(mode_of(&serde_json::json!({"mode": "agg"})), "agg");
    }
}
