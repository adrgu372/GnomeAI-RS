//! The coding-agent verification loop.
//!
//! Flow: detect the toolchain, run checks inside the sandbox, parse the output
//! into structured diagnostics, hand a compact summary back to the model, and
//! decide when to stop.
//!
//! The stopping criteria matter more than the running. Three ways out:
//!   - green
//!   - round budget exhausted
//!   - stalled (identical diagnostics two rounds running — the model is stuck
//!     and every further turn is wasted tokens)
//!
//! And one thing most implementations get wrong: run the checks *before* the
//! agent edits anything. Plenty of repositories are already red — failing
//! tests, clippy warnings nobody fixed. Without a baseline the agent chases
//! pre-existing errors forever and never reaches a state it considers done.
//! Hold it responsible for the delta, not the absolute.

use anyhow::Result;
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use crate::sandbox::{ExecOutput, SandboxPolicy, spawn_sandboxed_with_cancel};

// ---------------------------------------------------------------------------
// Diagnostics
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stage {
    /// Cheapest signal that the code is well-formed. Runs first, always.
    Check,
    Test,
    Lint,
    Format,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Stage::Check => "check",
            Stage::Test => "test",
            Stage::Lint => "lint",
            Stage::Format => "format",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    pub file: Option<PathBuf>,
    pub line: Option<u32>,
    pub column: Option<u32>,
    pub code: Option<String>,
    pub message: String,
}

impl Diagnostic {
    /// Identity used for dedup and for stall detection. Deliberately excludes
    /// the message text: rustc rewords the same error as surrounding code
    /// changes, and we do not want that counting as progress.
    fn fingerprint(&self) -> String {
        format!(
            "{}:{}:{}",
            self.file
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            self.line.unwrap_or(0),
            self.code.clone().unwrap_or_default(),
        )
    }
}

fn fingerprints(diags: &[Diagnostic]) -> HashSet<String> {
    diags.iter().map(Diagnostic::fingerprint).collect()
}

// ---------------------------------------------------------------------------
// Toolchain detection
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Toolchain {
    Rust,
    Node,
    Python,
    Go,
    Make,
    Unknown,
}

pub fn detect(root: &Path) -> Toolchain {
    let has = |f: &str| root.join(f).exists();

    // Order matters: a Rust project with a Makefile is still a Rust project.
    if has("Cargo.toml") {
        Toolchain::Rust
    } else if has("go.mod") {
        Toolchain::Go
    } else if has("package.json") {
        Toolchain::Node
    } else if has("pyproject.toml") || has("setup.py") || has("requirements.txt") {
        Toolchain::Python
    } else if has("Makefile") || has("makefile") {
        Toolchain::Make
    } else {
        Toolchain::Unknown
    }
}

pub struct StageCommand {
    pub stage: Stage,
    pub program: String,
    pub args: Vec<String>,
    /// If false, a failure here is reported but does not fail the run.
    pub blocking: bool,
}

fn cmd(stage: Stage, blocking: bool, program: &str, args: &[&str]) -> StageCommand {
    StageCommand {
        stage,
        program: program.to_string(),
        args: args.iter().map(|s| s.to_string()).collect(),
        blocking,
    }
}

impl Toolchain {
    pub fn stages(self) -> Vec<StageCommand> {
        match self {
            Toolchain::Rust => vec![
                // --message-format=json is the whole reason Rust is the easiest
                // language to build an agent for: structured diagnostics, no
                // regex, no guessing.
                cmd(
                    Stage::Check,
                    true,
                    "cargo",
                    &["check", "--all-targets", "--message-format=json"],
                ),
                cmd(Stage::Test, true, "cargo", &["test", "--no-fail-fast"]),
                cmd(
                    Stage::Lint,
                    false,
                    "cargo",
                    &["clippy", "--all-targets", "--message-format=json"],
                ),
                cmd(Stage::Format, false, "cargo", &["fmt", "--check"]),
            ],
            Toolchain::Go => vec![
                cmd(Stage::Check, true, "go", &["build", "./..."]),
                cmd(Stage::Test, true, "go", &["test", "./..."]),
                cmd(Stage::Lint, false, "go", &["vet", "./..."]),
            ],
            Toolchain::Node => vec![
                cmd(
                    Stage::Check,
                    true,
                    "npx",
                    &["tsc", "--noEmit", "--pretty", "false"],
                ),
                cmd(Stage::Test, true, "npm", &["test", "--silent"]),
            ],
            Toolchain::Python => vec![
                cmd(
                    Stage::Check,
                    true,
                    "python",
                    &["-m", "compileall", "-q", "."],
                ),
                cmd(
                    Stage::Test,
                    true,
                    "python",
                    &["-m", "pytest", "-q", "--tb=short"],
                ),
                cmd(Stage::Lint, false, "python", &["-m", "ruff", "check", "."]),
            ],
            Toolchain::Make => vec![
                cmd(Stage::Check, true, "make", &[]),
                cmd(Stage::Test, true, "make", &["test"]),
            ],
            Toolchain::Unknown => vec![],
        }
    }

    fn parse(self, out: &ExecOutput) -> Vec<Diagnostic> {
        match self {
            Toolchain::Rust => parse_cargo_json(&out.stdout),
            Toolchain::Go => parse_line_col(&out.stderr, Severity::Error),
            Toolchain::Node => parse_tsc(&out.stdout),
            _ => Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Parsers
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CargoLine {
    reason: String,
    message: Option<CargoMessage>,
}

#[derive(Deserialize)]
struct CargoMessage {
    level: String,
    message: String,
    code: Option<CargoCode>,
    #[serde(default)]
    spans: Vec<CargoSpan>,
}

#[derive(Deserialize)]
struct CargoCode {
    code: String,
}

#[derive(Deserialize)]
struct CargoSpan {
    file_name: String,
    line_start: u32,
    column_start: u32,
    is_primary: bool,
}

fn parse_cargo_json(stdout: &str) -> Vec<Diagnostic> {
    let mut out = Vec::new();

    for line in stdout.lines() {
        let Ok(parsed) = serde_json::from_str::<CargoLine>(line) else {
            continue; // cargo interleaves plain text; skip it
        };
        if parsed.reason != "compiler-message" {
            continue;
        }
        let Some(msg) = parsed.message else { continue };

        let severity = match msg.level.as_str() {
            "error" => Severity::Error,
            "warning" => Severity::Warning,
            _ => continue, // note/help are attached to a parent diagnostic
        };

        let primary = msg.spans.iter().find(|s| s.is_primary);

        out.push(Diagnostic {
            severity,
            file: primary.map(|s| PathBuf::from(&s.file_name)),
            line: primary.map(|s| s.line_start),
            column: primary.map(|s| s.column_start),
            code: msg.code.map(|c| c.code),
            message: msg.message,
        });
    }

    out
}

/// `file:line:col: message` — go, gcc, clang.
fn parse_line_col(text: &str, severity: Severity) -> Vec<Diagnostic> {
    use regex::Regex;
    let re = Regex::new(r"^(?P<file>[^\s:][^:]*):(?P<line>\d+):(?:(?P<col>\d+):)?\s*(?P<msg>.+)$")
        .expect("static regex");

    text.lines()
        .filter_map(|l| {
            let c = re.captures(l.trim())?;
            Some(Diagnostic {
                severity,
                file: Some(PathBuf::from(&c["file"])),
                line: c["line"].parse().ok(),
                column: c.name("col").and_then(|m| m.as_str().parse().ok()),
                code: None,
                message: c["msg"].to_string(),
            })
        })
        .collect()
}

/// `file(line,col): error TS2322: message`
fn parse_tsc(text: &str) -> Vec<Diagnostic> {
    use regex::Regex;
    let re = Regex::new(
        r"^(?P<file>[^(]+)\((?P<line>\d+),(?P<col>\d+)\):\s*(?P<sev>error|warning)\s+(?P<code>TS\d+):\s*(?P<msg>.+)$",
    )
    .expect("static regex");

    text.lines()
        .filter_map(|l| {
            let c = re.captures(l.trim())?;
            Some(Diagnostic {
                severity: if &c["sev"] == "error" {
                    Severity::Error
                } else {
                    Severity::Warning
                },
                file: Some(PathBuf::from(&c["file"])),
                line: c["line"].parse().ok(),
                column: c["col"].parse().ok(),
                code: Some(c["code"].to_string()),
                message: c["msg"].to_string(),
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Ranking
// ---------------------------------------------------------------------------

/// Dedupe, drop warnings when errors are present, and cap the count.
///
/// One type error in Rust cascades into dozens of downstream complaints. Send
/// the model all of them and it fixes symptoms; send it the first few real
/// errors and it fixes the cause.
fn rank(mut diags: Vec<Diagnostic>, limit: usize) -> Vec<Diagnostic> {
    let mut seen = HashSet::new();
    diags.retain(|d| seen.insert(d.fingerprint()));

    let has_errors = diags.iter().any(|d| d.severity == Severity::Error);
    if has_errors {
        diags.retain(|d| d.severity == Severity::Error);
    }

    diags.sort_by(|a, b| {
        a.severity
            .cmp(&b.severity)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.line.cmp(&b.line))
    });

    diags.truncate(limit);
    diags
}

// ---------------------------------------------------------------------------
// Running one pass
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct VerifyReport {
    pub toolchain_missing: bool,
    pub failed_stage: Option<Stage>,
    pub diagnostics: Vec<Diagnostic>,
    /// Tail of raw output, used only when the parser produced nothing.
    pub raw_tail: String,
    pub duration: Duration,
}

impl VerifyReport {
    pub fn is_green(&self) -> bool {
        self.failed_stage.is_none()
    }
}

pub async fn verify(root: &Path, policy: &SandboxPolicy) -> Result<VerifyReport> {
    verify_with_cancel(root, policy, &CancellationToken::new()).await
}

pub async fn verify_with_cancel(
    root: &Path,
    policy: &SandboxPolicy,
    cancel: &CancellationToken,
) -> Result<VerifyReport> {
    let started = Instant::now();
    let toolchain = detect(root);
    let stages = toolchain.stages();

    if stages.is_empty() {
        return Ok(VerifyReport {
            toolchain_missing: true,
            failed_stage: None,
            diagnostics: Vec::new(),
            raw_tail: String::new(),
            duration: started.elapsed(),
        });
    }

    for stage in stages {
        let out = spawn_sandboxed_with_cancel(policy, &stage.program, &stage.args, cancel).await?;

        let ok = out.exit_code == Some(0) && !out.timed_out;
        if ok {
            continue;
        }
        if !stage.blocking {
            continue;
        }

        let diagnostics = rank(toolchain.parse(&out), 8);
        let raw_tail = if diagnostics.is_empty() {
            tail(&out.stderr, 40) + &tail(&out.stdout, 20)
        } else {
            String::new()
        };

        return Ok(VerifyReport {
            toolchain_missing: false,
            failed_stage: Some(stage.stage),
            diagnostics,
            raw_tail,
            duration: started.elapsed(),
        });
    }

    Ok(VerifyReport {
        toolchain_missing: false,
        failed_stage: None,
        diagnostics: Vec::new(),
        raw_tail: String::new(),
        duration: started.elapsed(),
    })
}

fn tail(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}

// ---------------------------------------------------------------------------
// The loop
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum Outcome {
    /// Everything passes, or everything that was passing before still passes.
    Green,
    /// Ran out of rounds with work outstanding.
    Exhausted {
        rounds: usize,
        remaining: Vec<Diagnostic>,
    },
    /// Two consecutive rounds produced the same diagnostics. Further turns
    /// will not help; surface it to the user instead of burning budget.
    Stalled {
        rounds: usize,
        stuck_on: Vec<Diagnostic>,
    },
    /// The agent made things worse than the baseline. Caller should roll back.
    Regressed {
        rounds: usize,
        introduced: Vec<Diagnostic>,
    },
}

pub struct Loop {
    pub max_rounds: usize,
    /// Diagnostics present before the agent touched anything.
    baseline: HashSet<String>,
    previous: Option<HashSet<String>>,
    round: usize,
}

impl Loop {
    /// Establish the baseline. Call once, before the first patch is applied.
    pub async fn new(root: &Path, policy: &SandboxPolicy, max_rounds: usize) -> Result<Self> {
        let report = verify(root, policy).await?;
        Ok(Self {
            max_rounds,
            baseline: fingerprints(&report.diagnostics),
            previous: None,
            round: 0,
        })
    }

    /// Run one verification pass and decide whether to continue.
    ///
    /// Returns `Ok(None)` when the agent should keep working — the caller sends
    /// `render(&report)` back to the model and applies the next patch.
    pub async fn step(
        &mut self,
        root: &Path,
        policy: &SandboxPolicy,
    ) -> Result<(VerifyReport, Option<Outcome>)> {
        self.round += 1;
        let report = verify(root, policy).await?;
        let current = fingerprints(&report.diagnostics);

        // Only hold the agent responsible for what it introduced.
        let new: Vec<Diagnostic> = report
            .diagnostics
            .iter()
            .filter(|d| !self.baseline.contains(&d.fingerprint()))
            .cloned()
            .collect();

        if report.is_green() || new.is_empty() {
            return Ok((report, Some(Outcome::Green)));
        }

        if let Some(prev) = &self.previous {
            if *prev == current {
                let stuck_on = new.clone();
                return Ok((
                    report,
                    Some(Outcome::Stalled {
                        rounds: self.round,
                        stuck_on,
                    }),
                ));
            }
        }
        self.previous = Some(current);

        // Heuristic: a large jump in error count usually means the model
        // deleted something load-bearing rather than fixing anything.
        if new.len() > self.baseline.len().max(3) * 3 {
            return Ok((
                report,
                Some(Outcome::Regressed {
                    rounds: self.round,
                    introduced: new,
                }),
            ));
        }

        if self.round >= self.max_rounds {
            return Ok((
                report,
                Some(Outcome::Exhausted {
                    rounds: self.round,
                    remaining: new,
                }),
            ));
        }

        Ok((report, None))
    }
}

// ---------------------------------------------------------------------------
// Rendering for the model
// ---------------------------------------------------------------------------

/// Compact. The model does not need the ASCII art rustc draws — it needs file,
/// line, code and message. Sending the full rendered output costs thousands of
/// tokens per round and buys nothing.
pub fn render(report: &VerifyReport) -> String {
    if report.toolchain_missing {
        return "No supported project toolchain was detected; automatic verification was skipped."
            .into();
    }
    if report.is_green() {
        return format!(
            "All checks passed in {:.1}s.",
            report.duration.as_secs_f32()
        );
    }

    let stage = report.failed_stage.map(Stage::label).unwrap_or("?");

    if report.diagnostics.is_empty() {
        return format!(
            "Stage `{stage}` failed. Output tail:\n\n{}",
            report.raw_tail
        );
    }

    let mut s = format!(
        "Stage `{stage}` failed with {} diagnostic(s):\n\n",
        report.diagnostics.len()
    );

    for d in &report.diagnostics {
        let loc = match (&d.file, d.line, d.column) {
            (Some(f), Some(l), Some(c)) => format!("{}:{l}:{c}", f.display()),
            (Some(f), Some(l), None) => format!("{}:{l}", f.display()),
            (Some(f), _, _) => f.display().to_string(),
            _ => "<unknown>".into(),
        };
        let code = d
            .code
            .as_deref()
            .map(|c| format!(" [{c}]"))
            .unwrap_or_default();
        s.push_str(&format!("{loc}{code}  {}\n", d.message));
    }

    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cargo_diagnostics() {
        let line = r#"{"reason":"compiler-message","message":{"level":"error","message":"mismatched types","code":{"code":"E0308"},"spans":[{"file_name":"src/main.rs","line_start":42,"column_start":9,"is_primary":true}]}}"#;
        let d = parse_cargo_json(line);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].code.as_deref(), Some("E0308"));
        assert_eq!(d[0].line, Some(42));
    }

    #[test]
    fn ignores_non_message_lines() {
        let s = "{\"reason\":\"build-finished\",\"success\":false}\nnot json at all";
        assert!(parse_cargo_json(s).is_empty());
    }

    #[test]
    fn drops_warnings_when_errors_present() {
        let diags = vec![
            Diagnostic {
                severity: Severity::Warning,
                file: Some("a.rs".into()),
                line: Some(1),
                column: None,
                code: None,
                message: "unused".into(),
            },
            Diagnostic {
                severity: Severity::Error,
                file: Some("b.rs".into()),
                line: Some(2),
                column: None,
                code: None,
                message: "boom".into(),
            },
        ];
        let ranked = rank(diags, 8);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].severity, Severity::Error);
    }
}
