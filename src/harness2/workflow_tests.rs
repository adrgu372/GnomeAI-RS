//! End-to-end regressions for the paper's execution, evidence and adoption
//! contracts. A deterministic adapter plus a scripted proposer run the real
//! recursion code; no model service and no judge is involved.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::adapter::{Adapter, BenchPrompts, BenchSpec, RunView};
use super::library::{Experience, Library};
use super::proposer::{Proposer, Reply, ScriptedSession};
use super::remove_tree_all;
use super::schedule::{TaskOptions, run_domain, run_task};
use super::substrate::opencode_surface;
use super::tree::{self, Tree};

const BASE_SCRIPT: &str = "import csv\n\
import sys\n\
from decimal import Decimal\n\
\n\
with open(sys.argv[1]) as source:\n\
    amounts = [Decimal(row['amount']) for row in csv.DictReader(source)]\n\
total = sum((amount for amount in amounts if amount > 0), Decimal(0))\n\
print(f'Total: {total:.2f}')\n";

/// A deterministic local adapter: the "solver" sums the ledger with whatever
/// `scripts/summarize.py` the harness carries, so a harness edit changes the
/// delivered artifact exactly as upstream's demo adapter does.
struct DemoAdapter {
    root: PathBuf,
    surface: super::tree::SurfaceSpec,
    spec: BenchSpec,
    executions: AtomicU64,
    /// When false, the delivered artifact is deliberately empty so resume
    /// detection can be exercised.
    deliver: AtomicU64,
}

impl DemoAdapter {
    fn new(root: &Path) -> Self {
        let surface = opencode_surface();
        let base = root.join("base");
        let mut t = Tree::new();
        for a in surface.always_on {
            t.insert(a.to_string(), String::new());
        }
        t.insert(
            "systemprompt.md".into(),
            "Reconcile the ledger using python3 scripts/summarize.py.\n".into(),
        );
        t.insert("scripts/summarize.py".into(), BASE_SCRIPT.into());
        tree::write_tree(&surface, &t, &base, false).unwrap();
        for (task, amounts) in [
            ("ledger-1", "100.25\n-20.10\n5.00\n"),
            ("ledger-2", "35.50\n-10.25\n-5.00\n"),
        ] {
            let dir = root.join("sources").join(task);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("ledger.csv"), format!("amount\n{amounts}")).unwrap();
        }
        let prompts: BenchPrompts = serde_json::from_value(serde_json::json!({
            "work": "reconcile signed amounts in a transaction ledger"
        }))
        .unwrap();
        Self {
            root: root.to_path_buf(),
            surface,
            spec: BenchSpec {
                name: "demo".into(),
                runs_root: root.join("runs"),
                install_mode: "workspace_config".into(),
                prompts,
                domain_word: "accounting".into(),
                leak_globs: vec!["reference_answer.txt".into()],
                allow_subagents: false,
                proposer_deliverables: "contents".into(),
                state_deliverable: false,
                obs_trunc: 50_000,
                reserved_scripts: Vec::new(),
            },
            executions: AtomicU64::new(0),
            deliver: AtomicU64::new(1),
        }
    }

    fn task_files(&self, task_id: &str) -> PathBuf {
        self.root.join("sources").join(task_id)
    }
}

fn sum_ledger(path: &Path) -> f64 {
    let body = std::fs::read_to_string(path).unwrap_or_default();
    body.lines()
        .skip(1)
        .filter_map(|l| l.trim().parse::<f64>().ok())
        .sum()
}

impl Adapter for DemoAdapter {
    fn spec(&self) -> &BenchSpec {
        &self.spec
    }

    fn substrate_name(&self) -> &str {
        "opencode"
    }

    fn tasks_for_split(&self, _split: &str) -> Vec<String> {
        vec!["ledger-1".into(), "ledger-2".into()]
    }

    fn domain_of(&self, _task_id: &str) -> String {
        "accounting".into()
    }

    fn task_prompt(&self, _task_id: &str) -> String {
        "Sum every signed amount in ledger.csv. Return Total: with two decimal places.".into()
    }

    fn task_files(&self, task_id: &str) -> Option<PathBuf> {
        Some(DemoAdapter::task_files(self, task_id))
    }

    fn execute<'a>(
        &'a self,
        task_id: &'a str,
        harness_dir: Option<&'a Path>,
        run_id: &'a str,
        _timeout: Option<u64>,
    ) -> Pin<Box<dyn std::future::Future<Output = anyhow::Result<PathBuf>> + Send + 'a>> {
        Box::pin(async move {
            self.executions.fetch_add(1, Ordering::SeqCst);
            let run_dir = self.root.join("executions").join(run_id.replace('/', "__"));
            std::fs::create_dir_all(&run_dir)?;
            let base_dir = self.root.join("base");
            let script_dir = harness_dir.unwrap_or(&base_dir);
            let script = std::fs::read_to_string(script_dir.join("scripts/summarize.py"))
                .unwrap_or_else(|_| BASE_SCRIPT.to_string());
            // Execute the shipped helper for real via python3 when present;
            // otherwise fall back to the same arithmetic so the test stays
            // hermetic.
            let ledger = DemoAdapter::task_files(self, task_id).join("ledger.csv");
            let out = std::process::Command::new("python3")
                .arg("-c")
                .arg(&script)
                .arg(&ledger)
                .output();
            let stdout = match out {
                Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
                _ => {
                    let total = if script.contains("if amount > 0") {
                        std::fs::read_to_string(&ledger)
                            .unwrap_or_default()
                            .lines()
                            .skip(1)
                            .filter_map(|l| l.trim().parse::<f64>().ok())
                            .filter(|v| *v > 0.0)
                            .sum::<f64>()
                    } else {
                        sum_ledger(&ledger)
                    };
                    format!("Total: {total:.2}\n")
                }
            };
            if self.deliver.load(Ordering::SeqCst) == 1 {
                std::fs::write(run_dir.join("answer.txt"), &stdout)?;
            }
            std::fs::write(
                run_dir.join("trajectory.md"),
                format!("[ACTION] Run scripts/summarize.py on ledger.csv\n[OBSERVATION]\n{stdout}"),
            )?;
            Ok(run_dir)
        })
    }

    fn delivered(&self, run_dir: &Path) -> bool {
        run_dir.join("answer.txt").is_file()
    }

    fn trajectory(&self, run_dir: &Path, _obs_trunc: usize) -> anyhow::Result<String> {
        Ok(std::fs::read_to_string(run_dir.join("trajectory.md")).unwrap_or_default())
    }

    fn deliverable_text(&self, run_dir: &Path) -> anyhow::Result<String> {
        Ok(std::fs::read_to_string(run_dir.join("answer.txt")).unwrap_or_default())
    }

    fn deliverable_files(&self, run_dir: &Path) -> Option<BTreeMap<String, String>> {
        let text = std::fs::read_to_string(run_dir.join("answer.txt")).ok()?;
        let mut m = BTreeMap::new();
        m.insert("answer.txt".to_string(), text);
        Some(m)
    }

    fn base_harness(&self) -> PathBuf {
        self.root.join("base")
    }

    fn validate_tree(&self, harness: &Tree) -> Result<(), String> {
        if !harness.contains_key("scripts/summarize.py") {
            return Err("The local executor requires scripts/summarize.py".into());
        }
        Ok(())
    }

    fn score(&self, _run_dir: &Path, _task_id: &str) -> Option<serde_json::Value> {
        None
    }
}

/// Scripted edits and selections: it drops the positive-only filter, exactly
/// like upstream's `DemoProposer`.
struct DemoSession {
    calls: AtomicU64,
}

#[async_trait::async_trait]
impl ScriptedSession for DemoSession {
    async fn reply(
        &self,
        label: &str,
        workspace: &Path,
        _mission: &str,
    ) -> Result<(String, f64), String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if label.starts_with("select.") {
            // Pick the candidate whose rendered total includes the credits.
            let dir = workspace.join("context/candidates");
            let mut best = String::new();
            let mut best_total = f64::MIN;
            let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
                .map_err(|e| e.to_string())?
                .flatten()
                .map(|e| e.path())
                .collect();
            entries.sort();
            for path in entries {
                let body = std::fs::read_to_string(&path).unwrap_or_default();
                let total: f64 = body
                    .lines()
                    .find_map(|l| l.trim().strip_prefix("Total: "))
                    .and_then(|t| t.trim().parse().ok())
                    .unwrap_or(f64::MIN);
                if total > best_total {
                    best_total = total;
                    best = path
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                }
            }
            return Ok((
                format!(
                    "<why>the signed total matches the source ledger</why>\n```choice\n{best}\n```"
                ),
                0.0,
            ));
        }
        if label.starts_with("experience.") {
            return Ok((
                "<edits>\ncomponent: scripts/summarize.py\ncandidates: inv1\nchange: dropped \
                 the positive-only filter from the amount sum\nobserved: the probe total \
                 included the credits the base run omitted\nverdict: helped\n</edits>\n\n\
                 <experience>\ncue: When a ledger includes credits and debits\nlesson: Include \
                 both signs when summing in scripts/summarize.py.\nevidence: The \
                 scripts/summarize.py row -- removing the positive-only filter changed the \
                 probe total.\nconfidence: high\n</experience>"
                    .to_string(),
                1.0,
            ));
        }
        // A proposal: edit the harness in place and hand it back.
        let script = workspace.join("context/harness/scripts/summarize.py");
        let body = std::fs::read_to_string(&script).unwrap_or_else(|_| BASE_SCRIPT.to_string());
        std::fs::write(&script, body.replace(" if amount > 0", "")).map_err(|e| e.to_string())?;
        Ok((
            "<notes>Keep signed amounts: the base total omitted credits in the source \
             ledger.</notes>"
                .to_string(),
            1.0,
        ))
    }
}

fn temp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "h2-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    ));
    remove_tree_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test]
async fn budget_identity_and_adoption_hold_for_k1_and_k2() {
    for k in [1u32, 2] {
        let root = temp_root(&format!("wf{k}"));
        let adapter = Arc::new(DemoAdapter::new(&root));
        let session = Arc::new(DemoSession {
            calls: AtomicU64::new(0),
        });
        let prop = Arc::new(
            Proposer::new("gnomeai", "local/deterministic")
                .with_audit_dir(root.join("audit"))
                .with_scripted(session),
        );
        let libs: BTreeMap<String, Library> = ["agg", "seq"]
            .iter()
            .map(|m| {
                (
                    m.to_string(),
                    Library::new(root.join("library").join(m), "accounting", "opencode"),
                )
            })
            .collect();
        let options = TaskOptions {
            modes: vec!["agg".into(), "seq".into()],
            seed: None,
            tag: String::new(),
            libraries: libs,
            pool_policy: "finals".into(),
        };
        let res = run_task(
            adapter.clone(),
            prop.clone(),
            &adapter.spec().runs_root,
            "ledger-1",
            k,
            options,
        )
        .await
        .expect("task runs");
        assert_eq!(res.status, "ok", "k={k}: {:?}", res.errors);
        for mode in ["agg", "seq"] {
            let rec = &res.modes[mode];
            let ledger = &rec["ledger"];
            assert_eq!(ledger["R"], serde_json::json!(4 * k), "k={k} {mode} R");
            assert_eq!(ledger["P"], serde_json::json!(3 * k), "k={k} {mode} P");
            assert_eq!(rec["ok"], serde_json::json!(true));
        }
        // The adopted artifact carries the credits: 85.15 for ledger-1.
        let sel = &res.selections["agg"];
        let run_dir = PathBuf::from(sel["chosen_run_dir"].as_str().unwrap_or(""));
        let text = adapter.deliverable_text(&run_dir).unwrap_or_default();
        assert_eq!(text.trim(), "Total: 85.15", "k={k}");
        remove_tree_all(&root);
    }
}

#[tokio::test]
async fn a_second_run_is_skipped_and_a_broken_artifact_re_runs() {
    let root = temp_root("resume");
    let adapter = Arc::new(DemoAdapter::new(&root));
    let prop = Arc::new(
        Proposer::new("gnomeai", "local/deterministic").with_scripted(Arc::new(DemoSession {
            calls: AtomicU64::new(0),
        })),
    );
    let libs: BTreeMap<String, Library> = ["agg", "seq"]
        .iter()
        .map(|m| {
            (
                m.to_string(),
                Library::new(root.join("library").join(m), "accounting", "opencode"),
            )
        })
        .collect();
    let options = || TaskOptions {
        modes: vec!["agg".into(), "seq".into()],
        seed: None,
        tag: String::new(),
        libraries: libs.clone(),
        pool_policy: "finals".into(),
    };
    let first = run_task(
        adapter.clone(),
        prop.clone(),
        &adapter.spec().runs_root,
        "ledger-1",
        1,
        options(),
    )
    .await
    .unwrap();
    assert_eq!(first.status, "ok");
    let executions = adapter.executions.load(Ordering::SeqCst);
    let second = run_task(
        adapter.clone(),
        prop.clone(),
        &adapter.spec().runs_root,
        "ledger-1",
        1,
        options(),
    )
    .await
    .unwrap();
    assert_eq!(second.status, "skipped");
    assert_eq!(adapter.executions.load(Ordering::SeqCst), executions);

    // Deleting the adopted artifact forces a re-run rather than a silent skip.
    let run_dir = PathBuf::from(first.selections["agg"]["chosen_run_dir"].as_str().unwrap());
    std::fs::remove_file(run_dir.join("answer.txt")).unwrap();
    adapter.deliver.store(0, Ordering::SeqCst);
    let third = run_task(
        adapter.clone(),
        prop.clone(),
        &adapter.spec().runs_root,
        "ledger-1",
        1,
        options(),
    )
    .await
    .unwrap();
    assert_ne!(third.status, "skipped", "a broken artifact must not resume");
    assert!(
        adapter.executions.load(Ordering::SeqCst) > executions,
        "the rollouts should have run again"
    );
    remove_tree_all(&root);
}

#[tokio::test]
async fn the_library_threads_forward_and_a_lesson_is_stored() {
    let root = temp_root("library");
    let adapter = Arc::new(DemoAdapter::new(&root));
    let prop = Arc::new(
        Proposer::new("gnomeai", "local/deterministic").with_scripted(Arc::new(DemoSession {
            calls: AtomicU64::new(0),
        })),
    );
    let domain = run_domain(
        adapter.clone(),
        prop.clone(),
        &adapter.spec().runs_root,
        &root.join("library"),
        "accounting",
        &["ledger-1".to_string(), "ledger-2".to_string()],
        1,
        None,
        "",
        &["agg".to_string()],
    )
    .await;
    assert_eq!(domain["done"], serde_json::json!(2), "{domain}");
    let lib = Library::new(root.join("library/agg"), "accounting", "opencode");
    assert_eq!(
        lib.entries(),
        vec!["ledger-1".to_string(), "ledger-2".to_string()]
    );
    let text = std::fs::read_to_string(lib.entry_dir("ledger-1").join("EXPERIENCE.md")).unwrap();
    assert!(text.contains("both signs"), "{text}");
    let index = std::fs::read_to_string(lib.index()).unwrap();
    assert!(
        index.contains("ledger-1") && index.contains("ledger-2"),
        "{index}"
    );
    // The second task's evidence tree includes the first task's harness.
    let second_ctx = adapter
        .spec()
        .runs_root
        .join("demo-oc-agg1")
        .join("ledger-2")
        .join("context")
        .join("inv1")
        .join("library");
    assert!(
        second_ctx.is_dir(),
        "library evidence missing at {}",
        second_ctx.display()
    );
    remove_tree_all(&root);
}

#[tokio::test]
async fn an_invalid_proposal_is_rejected_and_retried_with_feedback() {
    struct BrokenThenFixed {
        seen_feedback: std::sync::Mutex<bool>,
    }

    #[async_trait::async_trait]
    impl ScriptedSession for BrokenThenFixed {
        async fn reply(
            &self,
            label: &str,
            workspace: &Path,
            mission: &str,
        ) -> Result<(String, f64), String> {
            if label.starts_with("select.") {
                return Ok(("```choice\nA\n```".into(), 0.0));
            }
            if label.starts_with("experience.") {
                return Ok(("NO_ENTRY".into(), 0.0));
            }
            if label.ends_with(".a1") {
                let script = workspace.join("context/harness/scripts/summarize.py");
                std::fs::write(&script, "def broken(:\n").map_err(|e| e.to_string())?;
                return Ok(("<notes>broken</notes>".into(), 1.0));
            }
            if mission.contains("PREVIOUS ATTEMPT WAS REJECTED") {
                *self.seen_feedback.lock().unwrap() = true;
            }
            let script = workspace.join("context/harness/scripts/summarize.py");
            std::fs::write(&script, BASE_SCRIPT).map_err(|e| e.to_string())?;
            Ok(("<notes>fixed</notes>".into(), 2.0))
        }
    }

    let root = temp_root("retry");
    let adapter = Arc::new(DemoAdapter::new(&root));
    let session = Arc::new(BrokenThenFixed {
        seen_feedback: std::sync::Mutex::new(false),
    });
    let prop =
        Arc::new(Proposer::new("gnomeai", "local/deterministic").with_scripted(session.clone()));
    let res = run_task(
        adapter.clone(),
        prop.clone(),
        &adapter.spec().runs_root,
        "ledger-1",
        1,
        TaskOptions {
            modes: vec!["agg".into()],
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(
        *session.seen_feedback.lock().unwrap(),
        "the retry never saw the rejection feedback"
    );
    let stages = res.modes["agg"]["stages"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let inv = stages
        .iter()
        .find(|s| s["label"] == serde_json::json!("inv1"))
        .expect("inv1 stage");
    assert_eq!(inv["fell_back"], serde_json::json!(false));
    assert_eq!(inv["attempts"], serde_json::json!(2));
    remove_tree_all(&root);
}

#[tokio::test]
async fn a_grader_verdict_in_a_proposal_is_refused() {
    struct Leaky;

    #[async_trait::async_trait]
    impl ScriptedSession for Leaky {
        async fn reply(
            &self,
            label: &str,
            workspace: &Path,
            _mission: &str,
        ) -> Result<(String, f64), String> {
            if label.starts_with("select.") {
                return Ok(("```choice\nA\n```".into(), 0.0));
            }
            let note = workspace.join("context/harness/LongTermMEMORY.md");
            std::fs::write(&note, "\"pass_rate\": 1\n").map_err(|e| e.to_string())?;
            Ok(("<notes>leak</notes>".into(), 0.0))
        }
    }

    let root = temp_root("leak");
    let adapter = Arc::new(DemoAdapter::new(&root));
    let prop =
        Arc::new(Proposer::new("gnomeai", "local/deterministic").with_scripted(Arc::new(Leaky)));
    let res = run_task(
        adapter.clone(),
        prop,
        &adapter.spec().runs_root,
        "ledger-1",
        1,
        TaskOptions {
            modes: vec!["agg".into()],
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let stages = res.modes["agg"]["stages"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let inv = stages
        .iter()
        .find(|s| s["label"] == serde_json::json!("inv1"))
        .expect("inv1 stage");
    assert_eq!(inv["fell_back"], serde_json::json!(true));
    let err = inv["error"].as_str().unwrap_or("");
    assert!(err.contains("verdict"), "error was: {err:?} stage: {inv}");
    remove_tree_all(&root);
}

#[test]
fn leak_guard_catches_the_reference_answer() {
    let root = temp_root("leakguard");
    std::fs::create_dir_all(root.join("source")).unwrap();
    std::fs::write(root.join("source/reference_answer.txt"), "42").unwrap();
    let err = super::context::assert_no_leak_paths(&root, &["reference_answer.txt".to_string()])
        .unwrap_err();
    assert!(err.to_string().contains("answer-key"));
    remove_tree_all(&root);
}
