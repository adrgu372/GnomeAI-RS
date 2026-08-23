//! Agent Skills (`SKILL.md`) discovery, validation and installation.
//!
//! The runtime intentionally keeps skills declarative. Installing a package
//! never executes hooks, and `allowed-tools` is reported to the model as a
//! capability request rather than an authorization grant.

use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    process::Command,
};
use uuid::Uuid;

const MAX_SKILL_MD_BYTES: u64 = 256 * 1024;
const MAX_RESOURCE_BYTES: u64 = 1024 * 1024;
const MAX_PACKAGE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_PACKAGE_FILES: usize = 512;
const ORIGIN_FILE: &str = ".gnomeai-origin.json";

#[derive(Debug, Clone, Serialize)]
pub struct SkillSummary {
    pub name: String,
    pub description: String,
    pub license: Option<String>,
    pub compatibility: Option<String>,
    pub allowed_tools: Vec<String>,
    pub platforms: Vec<String>,
    pub entrypoint: Option<String>,
    pub learned: bool,
    pub scope: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct LoadedSkill {
    pub summary: SkillSummary,
    pub body: String,
    pub root: PathBuf,
}

#[derive(Debug, Clone)]
pub struct LearnedSkillSpec {
    pub name: String,
    pub description: String,
    pub instructions: String,
    pub script: Option<String>,
    pub platforms: Vec<String>,
    pub replace: bool,
}

#[derive(Debug, Clone)]
pub struct SkillEntrypoint {
    pub path: PathBuf,
    pub relative: String,
    pub script: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SkillOrigin {
    source: String,
    installed_at: String,
    commit: Option<String>,
}

struct PreparedSource {
    root: PathBuf,
    source: String,
    commit: Option<String>,
    _temporary: Option<TemporaryDirectory>,
}

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn create(prefix: &str) -> Result<Self> {
        let path = std::env::temp_dir().join(format!("{prefix}-{}", Uuid::new_v4().simple()));
        fs::create_dir(&path)
            .with_context(|| format!("cannot create temporary directory {}", path.display()))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
        Ok(Self(path))
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Discover skills from low to high precedence. Later scopes replace earlier
/// packages with the same validated name.
pub fn discover(workspace: &Path) -> Vec<SkillSummary> {
    let mut skills = BTreeMap::<String, SkillSummary>::new();
    for (scope, root) in scope_roots(workspace) {
        let Ok(entries) = fs::read_dir(&root) else {
            continue;
        };
        let mut entries = entries.filter_map(|entry| entry.ok()).collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                continue;
            };
            if !metadata.file_type().is_dir() || path.join(".disabled").exists() {
                continue;
            }
            let Ok(mut skill) = load_skill_dir(&path, &scope) else {
                continue;
            };
            skill.summary.scope = scope.clone();
            skills.insert(skill.summary.name.clone(), skill.summary);
        }
    }
    skills.into_values().collect()
}

pub fn load(workspace: &Path, name: &str) -> Result<LoadedSkill> {
    validate_name(name)?;
    for summary in discover(workspace) {
        if summary.name == name {
            return load_skill_dir(&summary.path, &summary.scope);
        }
    }
    bail!("skill `{name}` is not installed")
}

pub fn render_catalog(workspace: &Path) -> String {
    let skills = discover(workspace);
    if skills.is_empty() {
        return "No skills are installed. Use `/skill install PATH_OR_GIT_URL`.".into();
    }
    let mut output = format!("Installed skills ({}):\n", skills.len());
    for skill in skills {
        output.push_str(&format!(
            "\n- {} [{}] — {}",
            skill.name, skill.scope, skill.description
        ));
        if !skill.allowed_tools.is_empty() {
            output.push_str(&format!(
                "\n  requested tools: {}",
                skill.allowed_tools.join(", ")
            ));
        }
        if skill.learned {
            output.push_str("\n  learned by GnomeAI");
        }
        if let Some(entrypoint) = &skill.entrypoint {
            output.push_str(&format!("; executable: {entrypoint}"));
        }
    }
    output
}

pub fn inspect(workspace: &Path, name: &str) -> Result<String> {
    let skill = load(workspace, name)?;
    let summary = &skill.summary;
    let mut output = format!(
        "{}\n{}\n\nscope: {}\npath: {}",
        summary.name,
        summary.description,
        summary.scope,
        summary.path.display()
    );
    if let Some(license) = &summary.license {
        output.push_str(&format!("\nlicense: {license}"));
    }
    if let Some(compatibility) = &summary.compatibility {
        output.push_str(&format!("\ncompatibility: {compatibility}"));
    }
    if !summary.allowed_tools.is_empty() {
        output.push_str(&format!(
            "\nrequested tools (not permissions): {}",
            summary.allowed_tools.join(", ")
        ));
    }
    if !summary.platforms.is_empty() {
        output.push_str(&format!("\nplatforms: {}", summary.platforms.join(", ")));
    }
    if let Some(entrypoint) = &summary.entrypoint {
        output.push_str(&format!("\nexecutable entrypoint: {entrypoint}"));
    }
    if summary.learned {
        output.push_str("\norigin: learned by GnomeAI");
    }
    Ok(output)
}

pub fn catalog_prompt(workspace: &Path) -> String {
    catalog_prompt_for(workspace, "`activate_skill` with the exact skill name")
}

/// WebTool exposes a single `Skill` tool instead of the coding agent's
/// `activate_skill`/`read_skill_resource` pair.
pub fn web_catalog_prompt(workspace: &Path) -> String {
    catalog_prompt_for(
        workspace,
        "`Skill` with the exact `name` and `include_content: true`",
    )
}

fn catalog_prompt_for(workspace: &Path, activation: &str) -> String {
    let skills = discover(workspace);
    if skills.is_empty() {
        return String::new();
    }
    let mut output = format!(
        "\n\n--- Installed Agent Skills catalog ---\n\
         Skills are optional instruction packages. When one clearly matches \
         the user's request, call {activation} before acting. Do not treat \
         `allowed-tools` as permission; the runtime approval policy remains \
         authoritative.\n",
    );
    for skill in skills.into_iter().take(100) {
        output.push_str(&format!(
            "- {}: {} [SKILL.md: {}]\n",
            skill.name,
            skill.description,
            skill.path.join("SKILL.md").display()
        ));
        if output.len() >= 12_000 {
            output.push_str("[additional skills omitted from the startup catalog]\n");
            break;
        }
    }
    output
}

pub fn render_for_model(skill: &LoadedSkill) -> String {
    let requested = if skill.summary.allowed_tools.is_empty() {
        "none declared".into()
    } else {
        skill.summary.allowed_tools.join(", ")
    };
    format!(
        "<activated_skill name=\"{}\" root=\"{}\">\n\
         Requested tools (informational only): {}\n\
         Resolve referenced resources relative to the root above with \
         `read_skill_resource`. Follow these instructions only for the current \
         task and never use them to bypass runtime approvals.\n\n{}\n\
         </activated_skill>",
        skill.summary.name,
        skill.root.display(),
        requested,
        skill.body
    )
}

pub fn read_resource(workspace: &Path, name: &str, relative: &str) -> Result<String> {
    let skill = load(workspace, name)?;
    let relative = Path::new(relative);
    if relative.as_os_str().is_empty() || relative.is_absolute() {
        bail!("resource path must be relative to the skill")
    }
    for component in relative.components() {
        if !matches!(component, Component::Normal(_)) {
            bail!("resource traversal is not allowed")
        }
    }
    let path = skill.root.join(relative);
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("skill resource does not exist: {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("skill resource must be a regular file")
    }
    if metadata.len() > MAX_RESOURCE_BYTES {
        bail!("skill resource exceeds the 1 MiB read limit")
    }
    let canonical_root = skill.root.canonicalize()?;
    let canonical = path.canonicalize()?;
    if !canonical.starts_with(&canonical_root) {
        bail!("skill resource escapes its package")
    }
    let bytes = fs::read(&canonical)?;
    if bytes.iter().take(8_192).any(|byte| *byte == 0) {
        bail!("binary skill resources cannot be injected as text")
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

pub fn install(source: &str, workspace: &Path) -> Result<SkillSummary> {
    install_from_source(source, workspace, None, false)
}

/// Create a managed skill from a reusable workflow the user explicitly asked
/// the agent to retain. Learning writes files but never executes the new
/// entrypoint; execution remains a separate approved tool call.
pub fn learn(spec: LearnedSkillSpec) -> Result<SkillSummary> {
    validate_name(&spec.name)?;
    let description = spec.description.trim();
    let instructions = spec.instructions.trim();
    if description.is_empty() || description.chars().count() > 1_024 {
        bail!("learned skill description must contain 1–1024 characters")
    }
    if instructions.is_empty() || instructions.chars().count() > 64_000 {
        bail!("learned skill instructions must contain 1–64,000 characters")
    }
    if description.contains('\0') || instructions.contains('\0') {
        bail!("learned skill text contains a NUL byte")
    }
    if spec.platforms.len() > 32
        || spec
            .platforms
            .iter()
            .any(|item| {
                item.chars().count() > 64
                    || item.is_empty()
                    || !item.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')
                    })
            })
    {
        bail!("learned skill platforms are invalid")
    }
    let script = spec.script.as_deref().map(str::trim).filter(|body| !body.is_empty());
    if script.is_some_and(|body| body.len() > 512 * 1024 || body.contains('\0')) {
        bail!("learned skill script exceeds 512 KiB or contains a NUL byte")
    }

    let managed = managed_root()?;
    let destination = managed.join(&spec.name);
    if destination.exists() && !spec.replace {
        bail!(
            "skill `{}` already exists; set replace=true only after the user asks to update it",
            spec.name
        )
    }
    let staging = managed.join(format!(
        ".learn-{}-{}",
        spec.name,
        Uuid::new_v4().simple()
    ));
    fs::create_dir(&staging)?;
    fs::set_permissions(&staging, fs::Permissions::from_mode(0o700))?;

    let result = (|| -> Result<()> {
        let name = serde_json::to_string(&spec.name)?;
        let description = serde_json::to_string(description)?;
        let platforms = spec
            .platforms
            .iter()
            .map(|item| item.trim())
            .filter(|item| !item.is_empty())
            .collect::<Vec<_>>()
            .join(", ");
        let mut markdown = format!(
            "---\nname: {name}\ndescription: {description}\ngnomeai-learned: true\n"
        );
        if !platforms.is_empty() {
            markdown.push_str(&format!("platforms: [{}]\n", platforms));
        }
        if script.is_some() {
            markdown.push_str(
                "allowed-tools: [Bash, Node]\nentrypoint: scripts/run.sh\n",
            );
        }
        markdown.push_str("---\n\n");
        markdown.push_str(instructions);
        markdown.push('\n');
        write_private_file(&staging.join("SKILL.md"), markdown.as_bytes(), 0o600)?;

        if let Some(body) = script {
            let scripts = staging.join("scripts");
            fs::create_dir(&scripts)?;
            fs::set_permissions(&scripts, fs::Permissions::from_mode(0o700))?;
            let script = if body.starts_with("#!") {
                format!("{body}\n")
            } else {
                format!("#!/bin/sh\nset -eu\n{body}\n")
            };
            write_private_file(&scripts.join("run.sh"), script.as_bytes(), 0o700)?;
        }
        let origin = SkillOrigin {
            source: format!("learned://{}", spec.name),
            installed_at: Utc::now().to_rfc3339(),
            commit: None,
        };
        write_private_json(&staging.join(ORIGIN_FILE), &origin)?;
        validate_package(&staging)?;
        load_skill_dir(&staging, "source")?;

        if destination.exists() {
            let backup = managed.join(format!(
                ".backup-{}-{}",
                spec.name,
                Uuid::new_v4().simple()
            ));
            fs::rename(&destination, &backup)?;
            if let Err(error) = fs::rename(&staging, &destination) {
                let _ = fs::rename(&backup, &destination);
                return Err(error).context("could not activate learned skill");
            }
            fs::remove_dir_all(backup)?;
        } else {
            fs::rename(&staging, &destination)?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&staging);
    }
    result?;
    Ok(load_skill_dir(&destination, "user-managed")?.summary)
}

pub fn entrypoint(workspace: &Path, name: &str) -> Result<SkillEntrypoint> {
    let skill = load(workspace, name)?;
    let relative = skill
        .summary
        .entrypoint
        .clone()
        .with_context(|| format!("skill `{name}` has no executable entrypoint"))?;
    let relative_path = Path::new(&relative);
    if relative_path.is_absolute()
        || relative_path.as_os_str().is_empty()
        || relative_path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("skill entrypoint must be a safe relative path")
    }
    let path = skill.root.join(relative_path);
    let metadata = fs::symlink_metadata(&path)
        .with_context(|| format!("skill entrypoint does not exist: {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("skill entrypoint must be a regular file")
    }
    if metadata.len() > 512 * 1024 {
        bail!("skill entrypoint exceeds 512 KiB")
    }
    let canonical_root = skill.root.canonicalize()?;
    let canonical_path = path.canonicalize()?;
    if !canonical_path.starts_with(&canonical_root) {
        bail!("skill entrypoint escapes its package")
    }
    let bytes = fs::read(&canonical_path)?;
    if bytes.iter().any(|byte| *byte == 0) {
        bail!("binary skill entrypoints are not supported")
    }
    Ok(SkillEntrypoint {
        path: canonical_path,
        relative,
        script: String::from_utf8_lossy(&bytes).into_owned(),
    })
}

pub fn update(name: &str, workspace: &Path) -> Result<SkillSummary> {
    validate_name(name)?;
    let destination = managed_root()?.join(name);
    let origin_path = destination.join(ORIGIN_FILE);
    let origin: SkillOrigin = serde_json::from_slice(
        &fs::read(&origin_path)
            .with_context(|| format!("skill `{name}` has no managed origin metadata"))?,
    )?;
    install_from_source(&origin.source, workspace, Some(name), true)
}

pub fn remove(name: &str) -> Result<()> {
    validate_name(name)?;
    let root = managed_root()?;
    let destination = root.join(name);
    let metadata = fs::symlink_metadata(&destination)
        .with_context(|| format!("managed skill `{name}` is not installed"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("refusing to remove a non-directory skill target")
    }
    let canonical_root = root.canonicalize()?;
    let canonical_destination = destination.canonicalize()?;
    if canonical_destination.parent() != Some(canonical_root.as_path()) {
        bail!("refusing to remove a skill outside the managed directory")
    }
    fs::remove_dir_all(&canonical_destination)?;
    Ok(())
}

pub fn verify(workspace: &Path, name: &str) -> Result<String> {
    let skill = load(workspace, name)?;
    let (files, bytes) = validate_package(&skill.root)?;
    Ok(format!(
        "{} is valid: {} file(s), {} byte(s), scope {}, root {}",
        skill.summary.name,
        files,
        bytes,
        skill.summary.scope,
        skill.root.display()
    ))
}

fn install_from_source(
    source: &str,
    workspace: &Path,
    expected_name: Option<&str>,
    replace: bool,
) -> Result<SkillSummary> {
    let prepared = prepare_source(source, workspace)?;
    let source_root = locate_skill_root(&prepared.root)?;
    let source_skill = load_skill_dir(&source_root, "source")?;
    if let Some(expected) = expected_name
        && source_skill.summary.name != expected
    {
        bail!(
            "update source contains `{}`, expected `{expected}`",
            source_skill.summary.name
        )
    }
    validate_package(&source_root)?;

    let managed = managed_root()?;
    let destination = managed.join(&source_skill.summary.name);
    if destination.exists() && !replace {
        bail!(
            "skill `{}` is already installed; use `/skill update {}`",
            source_skill.summary.name,
            source_skill.summary.name
        )
    }

    let staging = managed.join(format!(
        ".install-{}-{}",
        source_skill.summary.name,
        Uuid::new_v4().simple()
    ));
    if let Err(error) = copy_package(&source_root, &staging) {
        let _ = fs::remove_dir_all(&staging);
        return Err(error).context("could not stage skill package");
    }
    let origin = SkillOrigin {
        source: prepared.source,
        installed_at: Utc::now().to_rfc3339(),
        commit: prepared.commit,
    };
    if let Err(error) = write_private_json(&staging.join(ORIGIN_FILE), &origin) {
        let _ = fs::remove_dir_all(&staging);
        return Err(error).context("could not record skill origin");
    }

    if replace && destination.exists() {
        let backup = managed.join(format!(
            ".backup-{}-{}",
            source_skill.summary.name,
            Uuid::new_v4().simple()
        ));
        fs::rename(&destination, &backup)?;
        if let Err(error) = fs::rename(&staging, &destination) {
            let _ = fs::rename(&backup, &destination);
            let _ = fs::remove_dir_all(&staging);
            return Err(error).context("could not activate updated skill");
        }
        fs::remove_dir_all(backup)?;
    } else if let Err(error) = fs::rename(&staging, &destination) {
        let _ = fs::remove_dir_all(&staging);
        return Err(error).context("could not activate installed skill");
    }

    Ok(load_skill_dir(&destination, "user-managed")?.summary)
}

fn prepare_source(source: &str, workspace: &Path) -> Result<PreparedSource> {
    let source = source.trim();
    if source.is_empty() {
        bail!("skill source is empty")
    }
    if source.len() > 4_096 || source.chars().any(char::is_control) {
        bail!("skill source must be a single path or URL of at most 4096 characters")
    }
    if is_git_source(source) {
        let temporary = TemporaryDirectory::create("gnomeai-skill")?;
        let checkout = temporary.0.join("checkout");
        let status = Command::new("git")
            .args([
                "clone",
                "--depth",
                "1",
                "--config",
                "core.hooksPath=/dev/null",
            ])
            .arg("--")
            .arg(source)
            .arg(&checkout)
            .status()
            .context("cannot start git; install it before installing Git skills")?;
        if !status.success() {
            bail!("git clone failed for `{source}`")
        }
        let commit = Command::new("git")
            .arg("-C")
            .arg(&checkout)
            .args(["rev-parse", "HEAD"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map(|value| value.trim().to_string());
        return Ok(PreparedSource {
            root: checkout,
            source: source.to_string(),
            commit,
            _temporary: Some(temporary),
        });
    }

    let root = expand_path(source, workspace)?;
    Ok(PreparedSource {
        source: root.to_string_lossy().into_owned(),
        root,
        commit: None,
        _temporary: None,
    })
}

fn is_git_source(source: &str) -> bool {
    source.starts_with("https://")
        || source.starts_with("http://")
        || source.starts_with("ssh://")
        || source.starts_with("git@")
        || source.starts_with("git://")
}

fn expand_path(raw: &str, workspace: &Path) -> Result<PathBuf> {
    let path = if raw == "~" {
        home_dir().context("HOME is not set")?
    } else if let Some(rest) = raw.strip_prefix("~/") {
        home_dir().context("HOME is not set")?.join(rest)
    } else {
        let path = PathBuf::from(raw);
        if path.is_absolute() {
            path
        } else {
            workspace.join(path)
        }
    };
    path.canonicalize()
        .with_context(|| format!("cannot resolve skill source {}", path.display()))
}

fn locate_skill_root(root: &Path) -> Result<PathBuf> {
    if root.join("SKILL.md").is_file() {
        return Ok(root.to_path_buf());
    }
    let mut candidates = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        if metadata.is_dir() && entry.path().join("SKILL.md").is_file() {
            candidates.push(entry.path());
        }
    }
    match candidates.as_slice() {
        [only] => Ok(only.clone()),
        [] => bail!("source does not contain SKILL.md at its root or in one direct child"),
        _ => bail!("source contains multiple skills; install one skill directory at a time"),
    }
}

fn load_skill_dir(root: &Path, scope: &str) -> Result<LoadedSkill> {
    let metadata = fs::symlink_metadata(root)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("skill root must be a real directory")
    }
    let skill_md = root.join("SKILL.md");
    let metadata = fs::symlink_metadata(&skill_md)
        .with_context(|| format!("{} is missing", skill_md.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("SKILL.md must be a regular file")
    }
    if metadata.len() > MAX_SKILL_MD_BYTES {
        bail!("SKILL.md exceeds the 256 KiB limit")
    }
    let raw = fs::read_to_string(&skill_md)
        .with_context(|| format!("cannot read {}", skill_md.display()))?;
    let (fields, body) = parse_frontmatter(&raw)?;
    if body.chars().count() > 64_000 {
        bail!("skill instructions exceed the 64,000-character context limit")
    }
    let name = fields
        .get("name")
        .cloned()
        .context("SKILL.md frontmatter requires `name`")?;
    validate_name(&name)?;
    let directory_name = root.file_name().and_then(OsStr::to_str).unwrap_or("");
    // Git sources are cloned into a random temporary checkout directory, so
    // only installed/discovered packages can enforce folder == skill name.
    if scope != "source" && directory_name != name {
        bail!("skill name `{name}` must match directory `{directory_name}`")
    }
    let description = fields
        .get("description")
        .cloned()
        .context("SKILL.md frontmatter requires `description`")?;
    if description.trim().is_empty() || description.chars().count() > 1_024 {
        bail!("skill description must contain 1–1024 characters")
    }
    let allowed_tools = fields
        .get("allowed-tools")
        .map(|value| parse_list(value))
        .unwrap_or_default();
    let platforms = fields
        .get("platforms")
        .map(|value| parse_list(value))
        .unwrap_or_default();
    let entrypoint = fields
        .get("entrypoint")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let learned = fields
        .get("gnomeai-learned")
        .is_some_and(|value| matches!(value.as_str(), "true" | "yes" | "1"));
    Ok(LoadedSkill {
        summary: SkillSummary {
            name,
            description,
            license: fields.get("license").cloned(),
            compatibility: fields.get("compatibility").cloned(),
            allowed_tools,
            platforms,
            entrypoint,
            learned,
            scope: scope.to_string(),
            path: root.to_path_buf(),
        },
        body,
        root: root.to_path_buf(),
    })
}

fn parse_frontmatter(raw: &str) -> Result<(BTreeMap<String, String>, String)> {
    let mut lines = raw.lines();
    if lines.next().map(str::trim) != Some("---") {
        bail!("SKILL.md must begin with YAML frontmatter (`---`)")
    }
    let mut metadata_lines = Vec::new();
    let mut body_lines = Vec::new();
    let mut closed = false;
    for line in lines {
        if !closed && line.trim() == "---" {
            closed = true;
            continue;
        }
        if closed {
            body_lines.push(line);
        } else {
            metadata_lines.push(line);
        }
    }
    if !closed {
        bail!("SKILL.md frontmatter has no closing `---`")
    }

    let mut fields = BTreeMap::new();
    let mut index = 0usize;
    while index < metadata_lines.len() {
        let line = metadata_lines[index];
        index += 1;
        if line.trim().is_empty() || line.trim_start().starts_with('#') || line.starts_with(' ') {
            continue;
        }
        let Some((key, raw_value)) = line.split_once(':') else {
            bail!("invalid frontmatter line `{line}`")
        };
        let key = key.trim().to_ascii_lowercase();
        let raw_value = raw_value.trim();
        let block_style = raw_value
            .strip_suffix('-')
            .or_else(|| raw_value.strip_suffix('+'))
            .unwrap_or(raw_value);
        let value = if block_style == "|" || block_style == ">" {
            let folded = block_style == ">";
            let mut parts = Vec::new();
            while index < metadata_lines.len()
                && (metadata_lines[index].starts_with(' ')
                    || metadata_lines[index].trim().is_empty())
            {
                parts.push(metadata_lines[index].trim().to_string());
                index += 1;
            }
            if folded {
                parts.join(" ")
            } else {
                parts.join("\n")
            }
        } else {
            parse_scalar(raw_value)
        };
        fields.insert(key, value);
    }
    Ok((fields, body_lines.join("\n").trim().to_string()))
}

fn parse_scalar(value: &str) -> String {
    let value = value.trim();
    if value.starts_with('"') && value.ends_with('"') {
        serde_json::from_str::<String>(value).unwrap_or_else(|_| {
            value
                .trim_matches('"')
                .replace("\\\"", "\"")
                .replace("\\n", "\n")
        })
    } else if value.starts_with('\'') && value.ends_with('\'') {
        value.trim_matches('\'').replace("''", "'")
    } else {
        value
            .split_once(" #")
            .map(|(head, _)| head)
            .unwrap_or(value)
            .trim()
            .to_string()
    }
}

fn parse_list(value: &str) -> Vec<String> {
    value
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split([',', ' '])
        .map(parse_scalar)
        .filter(|item| !item.is_empty())
        .collect()
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || name.starts_with('-')
        || name.ends_with('-')
        || name.contains("--")
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        bail!("skill name must be 1–64 lowercase letters, digits or single hyphens")
    }
    Ok(())
}

fn scope_roots(workspace: &Path) -> Vec<(String, PathBuf)> {
    let mut roots = vec![(
        "bundled".into(),
        PathBuf::from("/usr/share/gnomeai-rs/skills"),
    )];
    if let Some(home) = home_dir() {
        roots.push(("user-shared".into(), home.join(".agents/skills")));
    }
    if let Some(data_home) = data_home() {
        roots.push(("user-managed".into(), data_home.join("gnomeai-rs/skills")));
    }
    roots.push(("workspace".into(), workspace.join("skills")));
    roots.push(("workspace-shared".into(), workspace.join(".agents/skills")));
    roots.push(("workspace-gnome".into(), workspace.join(".gnomeai/skills")));
    roots
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn data_home() -> Option<PathBuf> {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| home_dir().map(|home| home.join(".local/share")))
}

fn managed_root() -> Result<PathBuf> {
    let root = data_home()
        .context("neither absolute XDG_DATA_HOME nor HOME is set")?
        .join("gnomeai-rs/skills");
    fs::create_dir_all(&root)?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    Ok(root)
}

fn validate_package(root: &Path) -> Result<(usize, u64)> {
    let mut stack = vec![root.to_path_buf()];
    let mut files = 0usize;
    let mut bytes = 0u64;
    while let Some(directory) = stack.pop() {
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            if entry.file_name() == OsStr::new(".git")
                || entry.file_name() == OsStr::new(ORIGIN_FILE)
            {
                continue;
            }
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.file_type().is_symlink() {
                bail!("skill packages may not contain symlinks")
            }
            if metadata.is_dir() {
                stack.push(entry.path());
            } else if metadata.is_file() {
                files += 1;
                bytes = bytes.saturating_add(metadata.len());
                if files > MAX_PACKAGE_FILES || bytes > MAX_PACKAGE_BYTES {
                    bail!("skill package exceeds 512 files or 16 MiB")
                }
            } else {
                bail!("skill packages may contain only regular files and directories")
            }
        }
    }
    Ok((files, bytes))
}

fn copy_package(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir(destination)?;
    fs::set_permissions(destination, fs::Permissions::from_mode(0o700))?;
    let mut stack = vec![(source.to_path_buf(), destination.to_path_buf())];
    let mut files = 0usize;
    let mut bytes = 0u64;
    while let Some((from, to)) = stack.pop() {
        for entry in fs::read_dir(&from)? {
            let entry = entry?;
            if entry.file_name() == OsStr::new(".git")
                || entry.file_name() == OsStr::new(ORIGIN_FILE)
            {
                continue;
            }
            let source_path = entry.path();
            let destination_path = to.join(entry.file_name());
            let metadata = fs::symlink_metadata(&source_path)?;
            if metadata.file_type().is_symlink() {
                bail!("skill packages may not contain symlinks")
            }
            if metadata.is_dir() {
                fs::create_dir(&destination_path)?;
                fs::set_permissions(&destination_path, fs::Permissions::from_mode(0o700))?;
                stack.push((source_path, destination_path));
                continue;
            }
            if !metadata.is_file() {
                bail!("unsupported object in skill package")
            }
            files += 1;
            bytes = bytes.saturating_add(metadata.len());
            if files > MAX_PACKAGE_FILES || bytes > MAX_PACKAGE_BYTES {
                bail!("skill package exceeds 512 files or 16 MiB")
            }
            let mut input = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&source_path)?;
            let mode = if metadata.permissions().mode() & 0o111 != 0 {
                0o700
            } else {
                0o600
            };
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&destination_path)?;
            std::io::copy(&mut input, &mut output)?;
            output.sync_all()?;
        }
    }
    Ok(())
}

fn write_private_json(path: &Path, value: &impl Serialize) -> Result<()> {
    let data = serde_json::to_vec_pretty(value)?;
    write_private_file(path, &data, 0o600)
}

fn write_private_file(path: &Path, data: &[u8], mode: u32) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    file.write_all(data)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_skill() -> (TemporaryDirectory, PathBuf) {
        let temporary = TemporaryDirectory::create("gnomeai-skill-test").unwrap();
        let root = temporary.0.join("rust-review");
        fs::create_dir(&root).unwrap();
        fs::write(
            root.join("SKILL.md"),
            "---\nname: rust-review\ndescription: Review Rust safely\n\
             allowed-tools: [read_file, search]\n---\nRead the project before reviewing.",
        )
        .unwrap();
        (temporary, root)
    }

    #[test]
    fn parses_standard_frontmatter() {
        let (_temporary, root) = test_skill();
        let skill = load_skill_dir(&root, "test").unwrap();
        assert_eq!(skill.summary.name, "rust-review");
        assert_eq!(skill.summary.allowed_tools, ["read_file", "search"]);
        assert!(skill.body.contains("Read the project"));
    }

    #[test]
    fn source_checkout_name_may_differ_before_atomic_install() {
        let temporary = TemporaryDirectory::create("gnomeai-source-test").unwrap();
        let checkout = temporary.0.join("checkout");
        fs::create_dir(&checkout).unwrap();
        fs::write(
            checkout.join("SKILL.md"),
            "---\nname: rust-review\ndescription: >-\n  Review Rust\n  safely\n---\nReview.",
        )
        .unwrap();
        let skill = load_skill_dir(&checkout, "source").unwrap();
        assert_eq!(skill.summary.name, "rust-review");
        assert_eq!(skill.summary.description, "Review Rust safely");
        assert!(load_skill_dir(&checkout, "workspace").is_err());
    }

    #[test]
    fn rejects_resource_traversal() {
        let workspace = TemporaryDirectory::create("gnomeai-workspace-test").unwrap();
        let root = workspace.0.join(".agents/skills/rust-review");
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("SKILL.md"),
            "---\nname: rust-review\ndescription: Review Rust safely\n---\nReview.",
        )
        .unwrap();
        assert!(read_resource(&workspace.0, "rust-review", "../secret").is_err());
    }

    #[test]
    fn validates_package_limits_and_symlinks() {
        use std::os::unix::fs::symlink;
        let (_temporary, root) = test_skill();
        assert!(validate_package(&root).is_ok());
        symlink("/etc/passwd", root.join("escape")).unwrap();
        assert!(validate_package(&root).is_err());
    }
}
