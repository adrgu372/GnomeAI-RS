//! The c1..c8 component tree: read, validate, write, diff -- parametrised by
//! [`SurfaceSpec`]. Everything that differs between agent frameworks is data
//! on the spec (paths, caps, frontmatter rules, README seeds) plus a
//! per-surface hooks-schema validator; the shared logic exists once.
//!
//! The edit contract is *omitted-is-inherited*: a proposal emits only what it
//! changes.

use std::collections::BTreeMap;
use std::path::Path;

use serde::Serialize;

use super::{read_text_lossy, walk_files};

/// rel -> content. Ordered so validation errors and diffs come out stable.
pub type Tree = BTreeMap<String, String>;

/// Scripts the runtime itself provides; a proposal never has to ship them.
pub const RESERVED_SCRIPTS: &[&str] = &["read_doc.py", "pack.py", "unpack.py"];

/// How a skill's frontmatter `name:` relates to its directory.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SkillNameRule {
    /// The name must equal the directory or the skill is dropped at load.
    MustMatch,
    /// A present name overrides the directory (warn: trigger lines break).
    OverridesDir,
}

/// The editable surface of one agent framework: paths, caps and per-surface
/// rules.
#[derive(Clone)]
pub struct SurfaceSpec {
    pub name: &'static str,
    pub always_on: &'static [&'static str],
    pub always_on_max: usize,
    pub always_on_cap_reason: &'static str,
    pub skill_dir: &'static str,
    pub skill_name_rule: SkillNameRule,
    pub trigger_hint: &'static str,
    pub autotrigger_file: &'static str,
    pub trigger_line: &'static str,
    pub subagent_requires_mode: bool,
    pub plugin_dir: Option<&'static str>,
    pub hooks_file: Option<&'static str>,
    pub hook_events: &'static [&'static str],
    pub compile_skill_scripts: bool,
    pub reject_empty_always_on: bool,
    pub stubs: &'static [(&'static str, &'static str)],
}

fn component_ok(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Split `skills/<name>/SKILL.md` into its directory name, if it is a skill
/// file on this surface. Case-insensitive on `SKILL.md`, matching upstream's
/// `.lower()` handling of agent-produced paths.
pub fn skill_of(surface: &SurfaceSpec, rel: &str) -> Option<String> {
    let rest = rel.strip_prefix(surface.skill_dir)?.strip_prefix('/')?;
    let (dir, file) = rest.split_once('/')?;
    if !file.eq_ignore_ascii_case("SKILL.md") || !component_ok(dir) {
        return None;
    }
    Some(dir.to_string())
}

/// `skills/<name>/scripts/<file>` -- a helper carried with its skill.
pub fn skill_script_of(surface: &SurfaceSpec, rel: &str) -> Option<String> {
    let rest = rel.strip_prefix(surface.skill_dir)?.strip_prefix('/')?;
    let (dir, rest) = rest.split_once('/')?;
    let file = rest.strip_prefix("scripts/")?;
    if !component_ok(dir) || file.is_empty() {
        return None;
    }
    Some(dir.to_string())
}

/// `sub_agents/<name>/AGENT.md`.
pub fn subagent_of(rel: &str) -> Option<String> {
    let rest = rel.strip_prefix("sub_agents/")?;
    let (dir, file) = rest.split_once('/')?;
    if !file.eq_ignore_ascii_case("AGENT.md") || !component_ok(dir) {
        return None;
    }
    Some(dir.to_string())
}

/// `scripts/<name>.py` with a snake_case stem.
pub fn script_of(rel: &str) -> bool {
    let Some(rest) = rel.strip_prefix("scripts/") else {
        return false;
    };
    let Some(stem) = rest.strip_suffix(".py") else {
        return false;
    };
    let mut chars = stem.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c == '_')
}

/// Whether this path is on the editable surface. A predicate: it never raises.
pub fn is_allowed(surface: &SurfaceSpec, rel: &str) -> bool {
    if surface.always_on.contains(&rel) {
        return true;
    }
    if surface.hooks_file == Some(rel) {
        return true;
    }
    if skill_of(surface, rel).is_some()
        || skill_script_of(surface, rel).is_some()
        || subagent_of(rel).is_some()
        || script_of(rel)
    {
        return true;
    }
    match surface.plugin_dir {
        Some(dir) => match rel.strip_prefix(dir).and_then(|r| r.strip_prefix('/')) {
            Some(file) => match file.strip_suffix(".ts") {
                Some(stem) => component_ok(stem),
                None => false,
            },
            None => false,
        },
        None => false,
    }
}

/// Normalise a relative path, rejecting escapes before it is joined to a
/// directory.
pub fn normalize_rel(rel: &str) -> Result<String, String> {
    let r = rel.trim().replace('\\', "/");
    if r.starts_with('/') || r.split('/').any(|seg| seg == "..") {
        return Err(format!("path escapes the harness: {rel:?}"));
    }
    let r = r.trim_start_matches("./").trim_matches('/').to_string();
    if r.is_empty() {
        return Err(format!("path escapes the harness: {rel:?}"));
    }
    Ok(r)
}

pub fn skill_names(surface: &SurfaceSpec, tree: &Tree) -> Vec<String> {
    let mut names: Vec<String> = tree
        .keys()
        .filter_map(|rel| skill_of(surface, rel))
        .collect();
    names.sort();
    names.dedup();
    names
}

pub fn subagent_names(tree: &Tree) -> Vec<String> {
    let mut names: Vec<String> = tree.keys().filter_map(|rel| subagent_of(rel)).collect();
    names.sort();
    names.dedup();
    names
}

fn stub_paths(surface: &SurfaceSpec) -> Vec<&'static str> {
    surface.stubs.iter().map(|(rel, _)| *rel).collect()
}

/// rel -> content for every file on the editable surface, README stubs
/// excluded. Paths that exist on disk but are off the surface land in
/// `rejected` when the caller passes the vector.
pub fn read_tree(surface: &SurfaceSpec, dir: &Path) -> Tree {
    read_tree_with_rejects(surface, dir, None)
}

pub fn read_tree_with_rejects(
    surface: &SurfaceSpec,
    dir: &Path,
    rejected: Option<&mut Vec<String>>,
) -> Tree {
    let mut out = Tree::new();
    if !dir.is_dir() {
        return out;
    }
    let mut rejected = rejected;
    let stubs = stub_paths(surface);
    for rel in walk_files(dir) {
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if stubs.contains(&rel_str.as_str()) {
            continue;
        }
        if !is_allowed(surface, &rel_str) {
            if let Some(rej) = rejected.as_deref_mut() {
                rej.push(rel_str);
            }
            continue;
        }
        match read_text_lossy(&dir.join(&rel)) {
            Ok(body) => {
                out.insert(rel_str, body);
            }
            Err(_) => {
                out.insert(rel_str, String::new());
            }
        }
    }
    out
}

/// Whether `dir` holds an always-on file: the minimal "this tree exists" test.
pub fn is_tree(surface: &SurfaceSpec, dir: &Path) -> bool {
    dir.is_dir() && surface.always_on.iter().any(|a| dir.join(a).is_file())
}

/// Materialise an exact editable tree, with always-on files and optional stubs
/// present. Editable files already at `dest` are removed first; non-editable
/// files are preserved.
pub fn write_tree(
    surface: &SurfaceSpec,
    tree: &Tree,
    dest: &Path,
    stubs: bool,
) -> std::io::Result<()> {
    std::fs::create_dir_all(dest)?;
    let stub_set = stub_paths(surface);
    for rel in walk_files(dest) {
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if is_allowed(surface, &rel_str) || stub_set.contains(&rel_str.as_str()) {
            let _ = std::fs::remove_file(dest.join(&rel));
        }
    }
    for a in surface.always_on {
        let p = dest.join(a);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if !tree.contains_key(*a) && !p.exists() {
            std::fs::write(&p, "")?;
        }
    }
    for (rel, content) in tree {
        let norm = normalize_rel(rel).map_err(|e| std::io::Error::other(e))?;
        let p = dest.join(&norm);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&p, content)?;
    }
    if stubs {
        for (rel, body) in surface.stubs {
            let p = dest.join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if !p.exists() {
                std::fs::write(&p, body)?;
            }
        }
    }
    Ok(())
}

pub fn always_on_text(surface: &SurfaceSpec, tree: &Tree) -> String {
    surface
        .always_on
        .iter()
        .filter_map(|a| tree.get(*a))
        .cloned()
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Give every skill a trigger line in always-on text; return the names
/// repaired. A line that would push the body over the cap is skipped.
pub fn autotrigger(
    surface: &SurfaceSpec,
    tree: &mut Tree,
    always_on_max: Option<usize>,
) -> Vec<String> {
    let cap = always_on_max.unwrap_or(surface.always_on_max);
    let mut fixed = Vec::new();
    for name in skill_names(surface, tree) {
        let body = always_on_text(surface, tree);
        if body.contains(&name) {
            continue;
        }
        let fm: String = tree
            .get(&format!("{}/SKILL.md", surface.skill_dir))
            .map(|s| s.chars().take(600).collect())
            .unwrap_or_default();
        let desc = frontmatter_field(&fm, "description")
            .map(|d| d.trim_end_matches('.').to_string())
            .unwrap_or_else(|| format!("the {name} workflow applies"));
        let line = surface
            .trigger_line
            .replace("{name}", &name)
            .replace("{desc}", &desc);
        if body.len() + line.len() + 2 > cap {
            continue;
        }
        let cur = tree
            .get(surface.autotrigger_file)
            .cloned()
            .unwrap_or_default();
        let joined = if cur.trim().is_empty() {
            line
        } else {
            format!("{}\n{}", cur.trim_end(), line)
        };
        tree.insert(surface.autotrigger_file.to_string(), joined);
        fixed.push(name);
    }
    fixed
}

/// A single `key: value` line from a frontmatter-looking header block.
pub fn frontmatter_field(head: &str, key: &str) -> Option<String> {
    let mut value = None;
    for line in head.lines() {
        let trimmed = line.trim_end();
        if trimmed == "---" && value.is_some() {
            break;
        }
        if let Some(rest) = trimmed.strip_prefix(&format!("{key}:")) {
            if rest.starts_with(' ') {
                value = Some(rest.trim().to_string());
            }
        }
    }
    value
}

fn has_frontmatter_field(head: &str, key: &str) -> bool {
    frontmatter_field(head, key).is_some()
}

/// Validate the hooks file against the schema the framework dispatches; a
/// malformed one never fires rather than erroring, so the shape is enforced
/// where the proposer sees it.
fn check_hooks(surface: &SurfaceSpec, src: &str, errs: &mut Vec<String>) {
    let Some(hf) = surface.hooks_file else {
        return;
    };
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(src) else {
        errs.push(format!("{hf} is not valid JSON"));
        return;
    };
    let hooks = doc.get("hooks");
    let Some(hooks) = hooks else {
        errs.push(format!(
            r#"{hf} needs a top-level {{"hooks": {{"<Event>": [...]}}}} object"#
        ));
        return;
    };
    let Some(hooks) = hooks.as_object() else {
        errs.push(format!(
            r#"{hf} needs a top-level {{"hooks": {{"<Event>": [...]}}}} object"#
        ));
        return;
    };
    if hooks.is_empty() {
        errs.push(format!(
            r#"{hf} needs a top-level {{"hooks": {{"<Event>": [...]}}}} object"#
        ));
        return;
    }
    for (event, entries) in hooks {
        if !surface.hook_events.contains(&event.as_str()) {
            let mut known: Vec<&str> = surface.hook_events.to_vec();
            known.sort_unstable();
            errs.push(format!(
                "{hf}: unknown event {event:?}; the framework dispatches {} (PascalCase)",
                known.join(", ")
            ));
            continue;
        }
        let Some(entries) = entries.as_array() else {
            errs.push(format!(
                "{hf}: {event} must map to a LIST of matcher entries"
            ));
            continue;
        };
        for (i, entry) in entries.iter().enumerate() {
            let Some(inner) = entry.get("hooks").and_then(|h| h.as_array()) else {
                errs.push(format!("{hf}: {event}[{i}] needs a `hooks` list"));
                continue;
            };
            for h in inner {
                if h.get("type").and_then(|t| t.as_str()) != Some("command") {
                    errs.push(format!(
                        r#"{hf}: {event}[{i}] hook type must be "command" -- async/prompt/agent hooks are skipped as 'not supported yet'"#
                    ));
                } else if h
                    .get("command")
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .trim()
                    .is_empty()
                {
                    errs.push(format!("{hf}: {event}[{i}] has an empty `command`"));
                }
            }
        }
    }
}

/// A minimal Python compile gate: rejects unbalanced delimiters and empty
/// files. Mirrors the intent of upstream's `py_compile` check (a broken script
/// must be rejected before it burns a rollout) without a Python toolchain.
fn py_compiles(rel: &str, src: &str, errs: &mut Vec<String>) {
    let mut pairs = [(b'(', 0i64), (b'[', 0), (b'{', 0)];
    let mut quote: Option<u8> = None;
    let mut escape = false;
    for &b in src.as_bytes() {
        if escape {
            escape = false;
            continue;
        }
        if b == b'\\' {
            escape = true;
            continue;
        }
        if let Some(q) = quote {
            if b == q {
                quote = None;
            }
            continue;
        }
        match b {
            b'"' | b'\'' => quote = Some(b),
            b'(' | b'[' | b'{' => {
                if let Some(p) = pairs.iter_mut().find(|(open, _)| *open == b) {
                    p.1 += 1;
                }
            }
            b')' | b']' | b'}' => {
                let close = match b {
                    b')' => b'(',
                    b']' => b'[',
                    _ => b'{',
                };
                if let Some(p) = pairs.iter_mut().find(|(open, _)| *open == close) {
                    p.1 -= 1;
                }
            }
            _ => {}
        }
    }
    if quote.is_some() || escape {
        errs.push(format!(
            "{rel} does not compile: unterminated string literal"
        ));
        return;
    }
    for (open, depth) in pairs {
        if depth != 0 {
            errs.push(format!(
                "{rel} does not compile: unbalanced '{}'",
                open as char
            ));
        }
    }
}

fn check_skills(surface: &SurfaceSpec, tree: &Tree, body: &str, errs: &mut Vec<String>) -> usize {
    let skills = skill_names(surface, tree);
    for name in &skills {
        let path = format!("{}/{name}/SKILL.md", surface.skill_dir);
        let fm: String = tree
            .get(&path)
            .map(|s| s.chars().take(600).collect())
            .unwrap_or_default();
        if !has_frontmatter_field(&fm, "description") {
            errs.push(format!(
                "{path} needs a `description:` in its frontmatter, or the skill is dropped \
                 SILENTLY at load"
            ));
        }
        let name_line = frontmatter_field(&fm, "name");
        match surface.skill_name_rule {
            SkillNameRule::MustMatch => {
                if name_line.as_deref() != Some(name.as_str()) {
                    errs.push(format!(
                        "{path} needs frontmatter `name: {name}` matching its directory (a \
                         mismatch is dropped SILENTLY at load)"
                    ));
                }
            }
            SkillNameRule::OverridesDir => {
                if let Some(given) = name_line {
                    if given != *name {
                        errs.push(format!(
                            "{path} frontmatter `name: {given}` OVERRIDES the directory name at \
                             load, so every trigger line naming `{name}` stops matching; drop the \
                             `name:` line or make it `name: {name}`"
                        ));
                    }
                }
            }
        }
        if !body.contains(name) {
            errs.push(format!(
                "skill '{name}' is never named in the always-on text, so the agent will not \
                 know to invoke it -- an untriggered skill is dead weight. {}",
                surface.trigger_hint.replace("{name}", name)
            ));
        }
    }
    skills.len()
}

fn check_subagents(
    surface: &SurfaceSpec,
    tree: &Tree,
    allow_subagents: bool,
    errs: &mut Vec<String>,
) -> usize {
    let subs = subagent_names(tree);
    if !subs.is_empty() && !allow_subagents {
        errs.push(format!(
            "sub_agents are not available on this deployment: {subs:?}"
        ));
    }
    for name in &subs {
        let path = format!("sub_agents/{name}/AGENT.md");
        let fm: String = tree
            .get(&path)
            .map(|s| s.chars().take(600).collect())
            .unwrap_or_default();
        if surface.subagent_requires_mode {
            let mode_line = fm
                .lines()
                .any(|l| l.trim_end() == "mode: subagent" || l.trim_end() == "mode:subagent");
            if !mode_line {
                errs.push(format!("{path} needs frontmatter `mode: subagent`"));
            }
        }
        if !has_frontmatter_field(&fm, "description") {
            errs.push(format!("{path} needs a `description:`"));
        }
    }
    subs.len()
}

fn check_script_refs(
    surface: &SurfaceSpec,
    tree: &Tree,
    reserved: &dyn Fn(&str) -> bool,
    errs: &mut Vec<String>,
) {
    // References look like `python3 scripts/foo.py` or
    // `python3 skills/<skill>/scripts/foo.py`; capture what follows.
    let re = regex::Regex::new(&format!(
        r"python3?\s+((?:{}/[\w\-]+/)?scripts/[\w.\-]+\.py)",
        regex::escape(surface.skill_dir)
    ))
    .expect("script-ref regex");
    let have: Vec<&String> = tree
        .keys()
        .filter(|rel| script_of(rel) || skill_script_of(surface, rel).is_some())
        .collect();
    for (rel, src) in tree {
        if !(skill_of(surface, rel).is_some() || surface.always_on.contains(&rel.as_str())) {
            continue;
        }
        for cap in re.captures_iter(src) {
            let full = &cap[1];
            let base = full.rsplit('/').next().unwrap_or(full);
            if reserved(base) {
                continue;
            }
            if !have.iter().any(|h| h.as_str() == full) {
                errs.push(format!(
                    "{rel} invokes `{full}` but that file is not in the harness"
                ));
            }
        }
    }
}

/// The census of a validated tree.
#[derive(Debug, Default, Serialize)]
pub struct TreeCensus {
    pub always_on_chars: usize,
    pub n_files: usize,
    pub skills: usize,
    pub sub_agents: usize,
    pub scripts: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plugins: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hooks: Option<usize>,
}

/// Validate a proposed tree, listing every problem; `Err` joins them for the
/// retry feedback, `Ok` returns the census.
pub fn validate(
    surface: &SurfaceSpec,
    tree: &Tree,
    allow_subagents: bool,
    always_on_max: Option<usize>,
    reserved: &dyn Fn(&str) -> bool,
) -> Result<TreeCensus, String> {
    let mut errs = Vec::new();
    let cap = always_on_max.unwrap_or(surface.always_on_max);
    let body = always_on_text(surface, tree);
    if body.len() > cap {
        errs.push(format!(
            "always-on content is {} chars, over the {cap} cap; {}",
            body.len(),
            surface.always_on_cap_reason
        ));
    }
    if surface.reject_empty_always_on && body.trim().is_empty() {
        errs.push(format!(
            "{} is empty -- it is the only always-on file on this framework, so an empty one \
             is a harness that does not exist",
            surface.always_on[0]
        ));
    }
    let n_skills = check_skills(surface, tree, &body, &mut errs);
    let n_subs = check_subagents(surface, tree, allow_subagents, &mut errs);
    for (rel, src) in tree {
        if script_of(rel)
            || (surface.compile_skill_scripts
                && skill_script_of(surface, rel).is_some()
                && rel.ends_with(".py"))
        {
            py_compiles(rel, src, &mut errs);
        }
    }
    if let Some(hf) = surface.hooks_file {
        if let Some(src) = tree.get(hf) {
            check_hooks(surface, src, &mut errs);
        }
    }
    check_script_refs(surface, tree, reserved, &mut errs);
    if !errs.is_empty() {
        return Err(errs.join(" | "));
    }
    Ok(TreeCensus {
        always_on_chars: body.len(),
        n_files: tree.len(),
        skills: n_skills,
        sub_agents: n_subs,
        scripts: tree.keys().filter(|rel| script_of(rel)).count(),
        plugins: surface.plugin_dir.map(|dir| {
            tree.keys()
                .filter(|rel| {
                    rel.strip_prefix(dir)
                        .and_then(|r| r.strip_prefix('/'))
                        .map(|f| f.ends_with(".ts"))
                        .unwrap_or(false)
                })
                .count()
        }),
        hooks: surface
            .hooks_file
            .map(|hf| usize::from(tree.contains_key(hf))),
    })
}

/// Default reservation: the runtime-provided scripts.
pub fn reserved_default(base: &str) -> bool {
    RESERVED_SCRIPTS.contains(&base)
}

/// The three-way summary of what a proposal changed.
#[derive(Debug, Default, Serialize)]
pub struct DiffSummary {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub modified: Vec<String>,
    pub always_on_char_delta: i64,
}

impl DiffSummary {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.modified.is_empty()
    }
}

pub fn diff_summary(surface: &SurfaceSpec, before: &Tree, after: &Tree) -> DiffSummary {
    let added: Vec<String> = after
        .keys()
        .filter(|k| !before.contains_key(*k))
        .cloned()
        .collect();
    let removed: Vec<String> = before
        .keys()
        .filter(|k| !after.contains_key(*k))
        .cloned()
        .collect();
    let modified: Vec<String> = before
        .iter()
        .filter(|(k, v)| after.get(*k).map(|a| a != *v).unwrap_or(false))
        .map(|(k, _)| k.clone())
        .collect();
    let delta =
        always_on_text(surface, after).len() as i64 - always_on_text(surface, before).len() as i64;
    DiffSummary {
        added,
        removed,
        modified,
        always_on_char_delta: delta,
    }
}

/// Pairwise line-Jaccard over candidate trees, so collapse to one body is
/// visible.
pub fn diversity(trees: &BTreeMap<String, Tree>) -> serde_json::Value {
    let names: Vec<&String> = trees.keys().collect();
    if names.len() < 2 {
        return serde_json::json!({
            "n": names.len(),
            "n_distinct": names.len(),
            "mean_line_jaccard": 1.0,
        });
    }
    let sets: BTreeMap<&String, std::collections::BTreeSet<String>> = trees
        .iter()
        .map(|(name, t)| {
            let lines: std::collections::BTreeSet<String> = t
                .iter()
                .flat_map(|(rel, c)| {
                    c.lines()
                        .filter(|l| !l.trim().is_empty())
                        .map(move |l| format!("{rel}:{l}"))
                })
                .collect();
            (name, lines)
        })
        .collect();
    let mut sims = Vec::new();
    for i in 0..names.len() {
        for b in &names[i + 1..] {
            let a_set = sets.get(names[i]).cloned().unwrap_or_default();
            let b_set = sets.get(b).cloned().unwrap_or_default();
            let union = &a_set | &b_set;
            let inter = &a_set & &b_set;
            let sim = if union.is_empty() {
                1.0
            } else {
                inter.len() as f64 / union.len() as f64
            };
            sims.push(sim);
        }
    }
    let distinct = trees
        .values()
        .map(|t| t.iter().collect::<Vec<_>>())
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    serde_json::json!({
        "n": names.len(),
        "n_distinct": distinct,
        "mean_line_jaccard": sums(sims.iter().copied().collect::<Vec<f64>>()) / sims.len().max(1) as f64,
    })
}

fn sums(v: Vec<f64>) -> f64 {
    v.iter().sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness2::substrate::{codex_surface, opencode_surface};

    #[test]
    fn surface_predicates_match_upstream_shapes() {
        let oc = opencode_surface();
        assert!(is_allowed(&oc, "systemprompt.md"));
        assert!(is_allowed(&oc, "skills/doc_review/SKILL.md"));
        assert!(is_allowed(&oc, "skills/doc_review/scripts/parse.py"));
        assert!(is_allowed(&oc, "scripts/check_totals.py"));
        assert!(is_allowed(&oc, "plugin/trim.ts"));
        assert!(!is_allowed(&oc, "skills/doc_review/parse.py"));
        assert!(!is_allowed(&oc, "AGENTS.md"));
        assert!(!is_allowed(&oc, "notes.txt"));

        let cx = codex_surface();
        assert!(is_allowed(&cx, "AGENTS.md"));
        assert!(is_allowed(&cx, ".agents/skills/doc_review/SKILL.md"));
        assert!(is_allowed(&cx, ".codex/hooks.json"));
        assert!(!is_allowed(&cx, "plugin/trim.ts"));
    }

    #[test]
    fn normalize_rejects_escapes() {
        assert!(normalize_rel("../escape").is_err());
        assert!(normalize_rel("/absolute").is_err());
        assert_eq!(
            normalize_rel("./skills/x/SKILL.md").unwrap(),
            "skills/x/SKILL.md"
        );
    }

    #[test]
    fn validation_enforces_trigger_and_frontmatter() {
        let oc = opencode_surface();
        let mut tree = Tree::new();
        tree.insert("systemprompt.md".into(), "Reconcile the ledger.\n".into());
        tree.insert("guardrails.md".into(), String::new());
        tree.insert(
            "skills/doc_review/SKILL.md".into(),
            "---\nname: doc_review\ndescription: review documents\n---\nbody".into(),
        );
        // No trigger line: must fail.
        assert!(validate(&oc, &tree, false, None, &|b| reserved_default(b)).is_err());
        autotrigger(&oc, &mut tree, None);
        assert!(tree["guardrails.md"].contains("doc_review"));
        assert!(validate(&oc, &tree, false, None, &|b| reserved_default(b)).is_ok());
    }

    #[test]
    fn missing_script_ref_is_rejected() {
        let oc = opencode_surface();
        let mut tree = Tree::new();
        tree.insert(
            "systemprompt.md".into(),
            "Run python3 scripts/summarize.py first.\n".into(),
        );
        let err = validate(&oc, &tree, false, None, &|b| reserved_default(b)).unwrap_err();
        assert!(err.contains("summarize.py"), "got: {err}");
        tree.insert("scripts/summarize.py".into(), "print('hi')\n".into());
        assert!(validate(&oc, &tree, false, None, &|b| reserved_default(b)).is_ok());
    }

    #[test]
    fn broken_python_is_rejected() {
        let oc = opencode_surface();
        let mut tree = Tree::new();
        tree.insert("systemprompt.md".into(), "ok".into());
        tree.insert("scripts/broken.py".into(), "def broken(:\n".into());
        let err = validate(&oc, &tree, false, None, &|b| reserved_default(b)).unwrap_err();
        assert!(err.contains("unbalanced"), "got: {err}");
    }

    #[test]
    fn hooks_schema_is_enforced() {
        let cx = codex_surface();
        let mut tree = Tree::new();
        tree.insert("AGENTS.md".into(), "Standing rules.".into());
        tree.insert(
            ".codex/hooks.json".into(),
            r#"{"hooks": {"NotAnEvent": []}}"#.into(),
        );
        let err = validate(&cx, &tree, false, None, &|b| reserved_default(b)).unwrap_err();
        assert!(err.contains("unknown event"), "got: {err}");
    }

    #[test]
    fn diff_and_diversity() {
        let oc = opencode_surface();
        let mut a = Tree::new();
        a.insert("systemprompt.md".into(), "one".into());
        let mut b = a.clone();
        b.insert("scripts/x.py".into(), "print(1)".into());
        let d = diff_summary(&oc, &a, &b);
        assert_eq!(d.added, vec!["scripts/x.py".to_string()]);
        assert!(d.is_empty() == false);
        let mut trees = BTreeMap::new();
        trees.insert("a".to_string(), a.clone());
        trees.insert("b".to_string(), a);
        let div = diversity(&trees);
        assert_eq!(div["n_distinct"], 1);
        assert_eq!(div["mean_line_jaccard"], serde_json::json!(1.0));
    }
}
