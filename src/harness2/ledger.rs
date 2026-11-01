//! Budget accounting: R = 4k rollouts and P = 3k improvement stages per mode.
//!
//! Base executions count toward each mode. Retries are separate invocation
//! counts; selection and reflection are recorded outside this ledger.

use serde::Serialize;

#[derive(Debug, Clone)]
pub struct Entry {
    pub kind: &'static str,
    pub label: String,
    pub usd: f64,
    pub cached: bool,
    pub attempts: u32,
}

pub fn expected_rollouts(k: u32) -> u32 {
    4 * k
}

pub fn expected_sessions(k: u32) -> u32 {
    3 * k
}

/// Everything one mode spent on one task, recorded as it happens.
#[derive(Debug)]
pub struct Ledger {
    pub mode: String,
    pub task_id: String,
    pub k: u32,
    pub entries: Vec<Entry>,
}

impl Ledger {
    pub fn new(mode: &str, task_id: &str, k: u32) -> Self {
        Self {
            mode: mode.into(),
            task_id: task_id.into(),
            k,
            entries: Vec::new(),
        }
    }

    pub fn rollout(&mut self, label: &str) {
        self.entries.push(Entry {
            kind: "rollout",
            label: label.into(),
            usd: 0.0,
            cached: false,
            attempts: 1,
        });
    }

    pub fn session(&mut self, label: &str, usd: f64, cached: bool, attempts: u32) {
        self.entries.push(Entry {
            kind: "session",
            label: label.into(),
            usd,
            cached,
            attempts,
        });
    }

    fn count(&self, kind: &str) -> u32 {
        self.entries.iter().filter(|e| e.kind == kind).count() as u32
    }

    pub fn rollouts(&self) -> u32 {
        self.count("rollout")
    }

    pub fn sessions(&self) -> u32 {
        self.count("session")
    }

    pub fn usd(&self) -> f64 {
        round6(self.entries.iter().map(|e| e.usd).sum())
    }

    pub fn rollout_usd(&self) -> f64 {
        round6(
            self.entries
                .iter()
                .filter(|e| e.kind == "rollout")
                .map(|e| e.usd)
                .sum(),
        )
    }

    pub fn session_usd(&self) -> f64 {
        round6(
            self.entries
                .iter()
                .filter(|e| e.kind == "session")
                .map(|e| e.usd)
                .sum(),
        )
    }

    pub fn labels(&self, kind: &str) -> Vec<String> {
        self.entries
            .iter()
            .filter(|e| e.kind == kind)
            .map(|e| e.label.clone())
            .collect()
    }

    /// Raise unless the recorded entries show exactly R=4k and P=3k with no
    /// duplicate label. Duplicates are checked before the counts, because a
    /// duplicate can satisfy them.
    pub fn assert_identity(&self) -> Result<(), String> {
        let (want_r, want_p) = (expected_rollouts(self.k), expected_sessions(self.k));
        let rl = self.labels("rollout");
        let sl = self.labels("session");
        let mut dupes: Vec<String> = rl
            .iter()
            .filter(|l| rl.iter().filter(|x| *x == *l).count() > 1)
            .chain(
                sl.iter()
                    .filter(|l| sl.iter().filter(|x| *x == *l).count() > 1),
            )
            .cloned()
            .collect();
        dupes.sort();
        dupes.dedup();
        if dupes.is_empty() && self.rollouts() == want_r && self.sessions() == want_p {
            return Ok(());
        }
        let mut detail = vec![format!(
            "{}-{} {}: R={} (want {want_r}), P={} (want {want_p})",
            self.mode,
            self.k,
            self.task_id,
            self.rollouts(),
            self.sessions()
        )];
        detail.push(format!("  rollouts: {}", join_or(&rl, "(none)")));
        detail.push(format!("  sessions: {}", join_or(&sl, "(none)")));
        if !dupes.is_empty() {
            detail.push(format!(
                "  DUPLICATE labels: {dupes:?} -- a repeated label means two stages wrote the \
                 same run id and one silently overwrote the other, so a candidate the ledger \
                 counts was never actually produced"
            ));
        }
        Err(detail.join("\n"))
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "mode": self.mode,
            "task_id": self.task_id,
            "k": self.k,
            "R": self.rollouts(),
            "expected_R": expected_rollouts(self.k),
            "P": self.sessions(),
            "session_attempts": self.entries.iter().filter(|e| e.kind == "session").map(|e| e.attempts).sum::<u32>(),
            "expected_P": expected_sessions(self.k),
            "usd": self.usd(),
            "rollout_usd": self.rollout_usd(),
            "session_usd": self.session_usd(),
            "rollouts": self.labels("rollout"),
            "sessions": self.labels("session"),
        })
    }
}

pub fn round6(v: f64) -> f64 {
    (v * 1_000_000.0).round() / 1_000_000.0
}

fn join_or(items: &[String], empty: &str) -> String {
    if items.is_empty() {
        empty.to_string()
    } else {
        items.join(", ")
    }
}

/// A serialized ledger summary handed to reports.
#[derive(Debug, Serialize)]
pub struct LedgerJson(pub serde_json::Value);

#[cfg(test)]
mod tests {
    use super::*;

    fn full_ledger(k: u32) -> Ledger {
        let mut l = Ledger::new("agg", "t1", k);
        for i in 1..=k {
            l.rollout(&format!("base{i}"));
        }
        for i in 1..=k {
            for pre in ["inv", "dom"] {
                l.session(&format!("{pre}{i}"), 0.5, false, 1);
            }
        }
        for i in 1..=k {
            l.rollout(&format!("inv{i}"));
            l.rollout(&format!("dom{i}"));
        }
        for i in 1..=k {
            l.session(&format!("agg{i}"), 0.5, false, 1);
        }
        for i in 1..=k {
            l.rollout(&format!("agg{i}"));
        }
        l
    }

    #[test]
    fn budget_identity_holds_at_k1_and_k2() {
        for k in [1u32, 2] {
            let l = full_ledger(k);
            assert_eq!(l.rollouts(), expected_rollouts(k));
            assert_eq!(l.sessions(), expected_sessions(k));
            assert!(l.assert_identity().is_ok());
        }
    }

    #[test]
    fn duplicates_are_reported_before_counts() {
        let mut l = full_ledger(1);
        // Extra entry with a duplicated label satisfies the counts but must fail.
        l.rollout("base1");
        assert!(l.assert_identity().is_err());
    }
}
