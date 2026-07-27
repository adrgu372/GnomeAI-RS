//! Parser and transactional applier for the coding agent's patch format.

use anyhow::{Context, Result, bail};
use std::{
    collections::BTreeMap,
    ffi::{CString, OsStr},
    fs::{File, OpenOptions},
    io::Write,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    },
    path::{Component, Path, PathBuf},
};

const MAX_PATCH_BYTES: usize = 2 * 1024 * 1024;
const MAX_PATCH_LINES: usize = 100_000;
const MAX_PATCH_ACTIONS: usize = 256;
const MAX_PATCH_HUNKS: usize = 4_096;

#[derive(Debug, Clone)]
pub struct Patch {
    source: String,
    actions: Vec<Action>,
}

#[derive(Debug, Clone)]
enum Action {
    Add { path: PathBuf, content: String },
    Delete { path: PathBuf },
    Update { path: PathBuf, hunks: Vec<Hunk> },
}

#[derive(Debug, Clone)]
struct Hunk {
    marker: Option<String>,
    lines: Vec<HunkLine>,
}

#[derive(Debug, Clone)]
enum HunkLine {
    Context(String),
    Remove(String),
    Add(String),
}

#[derive(Debug)]
pub struct AppliedPatch {
    pub files_changed: Vec<PathBuf>,
    pub diff: String,
    pub changes: Vec<AppliedFile>,
}

#[derive(Debug)]
pub struct AppliedFile {
    /// Workspace-relative path.
    pub path: PathBuf,
    pub before: Option<String>,
    pub after: Option<String>,
    pub diff: String,
}

#[derive(Debug)]
struct WorkingFile {
    absolute: PathBuf,
    before: Option<String>,
    after: Option<String>,
}

pub fn parse(text: &str) -> Result<Patch> {
    if text.len() > MAX_PATCH_BYTES {
        bail!("patch exceeds the 2 MiB safety limit");
    }
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() > MAX_PATCH_LINES {
        bail!("patch exceeds the 100,000-line safety limit");
    }
    if lines.first().copied() != Some("*** Begin Patch") {
        bail!("patch must start with `*** Begin Patch`");
    }
    if lines.last().copied() != Some("*** End Patch") {
        bail!("patch must end with `*** End Patch`");
    }

    let mut actions = Vec::new();
    let mut i = 1usize;
    while i + 1 < lines.len() {
        let line = lines[i];
        if line.trim().is_empty() {
            i += 1;
            continue;
        }

        if let Some(raw_path) = line.strip_prefix("*** Add File: ") {
            let path = parse_path(raw_path)?;
            i += 1;
            let mut content = Vec::new();
            while i + 1 < lines.len() && !lines[i].starts_with("*** ") {
                let Some(value) = lines[i].strip_prefix('+') else {
                    bail!("line {} in an Add File section must start with `+`", i + 1);
                };
                content.push(value);
                i += 1;
            }
            let mut content = content.join("\n");
            if !content.is_empty() {
                content.push('\n');
            }
            actions.push(Action::Add { path, content });
            continue;
        }

        if let Some(raw_path) = line.strip_prefix("*** Delete File: ") {
            actions.push(Action::Delete {
                path: parse_path(raw_path)?,
            });
            i += 1;
            continue;
        }

        if let Some(raw_path) = line.strip_prefix("*** Update File: ") {
            let path = parse_path(raw_path)?;
            i += 1;
            let mut hunks = Vec::new();
            let mut current: Option<Hunk> = None;

            while i + 1 < lines.len() && !lines[i].starts_with("*** ") {
                let line = lines[i];
                if let Some(marker) = line.strip_prefix("@@") {
                    if let Some(hunk) = current.take() {
                        if !hunk.lines.is_empty() {
                            hunks.push(hunk);
                        }
                    }
                    current = Some(Hunk {
                        marker: non_empty(marker.trim()),
                        lines: Vec::new(),
                    });
                    i += 1;
                    continue;
                }

                let hunk = current.get_or_insert_with(|| Hunk {
                    marker: None,
                    lines: Vec::new(),
                });
                let Some(prefix) = line.chars().next() else {
                    bail!(
                        "blank patch line {} needs a context prefix (` `, `+`, or `-`)",
                        i + 1
                    );
                };
                let value = line[prefix.len_utf8()..].to_string();
                match prefix {
                    ' ' => hunk.lines.push(HunkLine::Context(value)),
                    '-' => hunk.lines.push(HunkLine::Remove(value)),
                    '+' => hunk.lines.push(HunkLine::Add(value)),
                    _ => bail!(
                        "line {} in an Update File section needs a context prefix",
                        i + 1
                    ),
                }
                i += 1;
            }

            if let Some(hunk) = current {
                if !hunk.lines.is_empty() {
                    hunks.push(hunk);
                }
            }
            if hunks.is_empty() {
                bail!("Update File section for {} has no hunks", path.display());
            }
            actions.push(Action::Update { path, hunks });
            continue;
        }

        bail!("unknown patch directive on line {}: {line}", i + 1);
    }

    if actions.is_empty() {
        bail!("patch contains no file operations");
    }
    if actions.len() > MAX_PATCH_ACTIONS {
        bail!("patch exceeds the 256-file-operation safety limit");
    }
    let hunks = actions
        .iter()
        .map(|action| match action {
            Action::Update { hunks, .. } => hunks.len(),
            _ => 0,
        })
        .sum::<usize>();
    if hunks > MAX_PATCH_HUNKS {
        bail!("patch exceeds the 4,096-hunk safety limit");
    }

    Ok(Patch {
        source: text.to_string(),
        actions,
    })
}

pub fn apply(root: &Path, patch: &Patch) -> Result<AppliedPatch> {
    let root = root
        .canonicalize()
        .with_context(|| format!("cannot resolve workspace {}", root.display()))?;
    let mut working: BTreeMap<PathBuf, WorkingFile> = BTreeMap::new();

    for action in &patch.actions {
        let path = match action {
            Action::Add { path, .. } | Action::Delete { path } | Action::Update { path, .. } => {
                path
            }
        };
        let absolute = resolve_path(&root, path)?;
        if !working.contains_key(path) {
            let before = read_text_if_present(&absolute)?;
            working.insert(
                path.clone(),
                WorkingFile {
                    absolute,
                    after: before.clone(),
                    before,
                },
            );
        }
        let file = working.get_mut(path).expect("working file was inserted");

        match action {
            Action::Add { content, .. } => {
                if file.after.is_some() {
                    bail!("cannot add {}; it already exists", path.display());
                }
                file.after = Some(content.clone());
            }
            Action::Delete { .. } => {
                if file.after.is_none() {
                    bail!("cannot delete {}; it does not exist", path.display());
                }
                file.after = None;
            }
            Action::Update { hunks, .. } => {
                let Some(content) = file.after.as_mut() else {
                    bail!("cannot update {}; it does not exist", path.display());
                };
                *content = apply_hunks(path, content, hunks)?;
            }
        }
    }

    let mut changes = Vec::new();
    for (path, file) in working {
        if file.before == file.after {
            continue;
        }
        changes.push((
            AppliedFile {
                path,
                before: file.before,
                after: file.after,
                diff: patch.source.clone(),
            },
            file.absolute,
        ));
    }
    if changes.is_empty() {
        bail!("patch made no changes");
    }

    let mut committed: Vec<usize> = Vec::new();
    for (index, (change, _absolute)) in changes.iter().enumerate() {
        let result = match &change.after {
            Some(content) => replace_file_beneath(&root, &change.path, content),
            None => remove_file_beneath(&root, &change.path, false),
        };
        if let Err(error) = result {
            let mut rollback_errors = Vec::new();
            for committed_index in committed.into_iter().rev() {
                let (old, _) = &changes[committed_index];
                if let Err(rollback_error) = restore_file_if_unchanged(
                    &root,
                    &old.path,
                    old.after.as_deref(),
                    old.before.as_deref(),
                ) {
                    rollback_errors.push(format!("{}: {rollback_error}", old.path.display()));
                }
            }
            if !rollback_errors.is_empty() {
                bail!(
                    "patch write failed ({error}); rollback deliberately left concurrently \
                     modified files untouched: {}",
                    rollback_errors.join("; ")
                );
            }
            return Err(error).context("patch was rolled back after a write failed");
        }
        committed.push(index);
    }

    let files_changed = changes
        .iter()
        .map(|(_, absolute)| absolute.clone())
        .collect();
    let changes = changes.into_iter().map(|(change, _)| change).collect();

    Ok(AppliedPatch {
        files_changed,
        diff: patch.source.clone(),
        changes,
    })
}

/// Resolve a workspace-relative path without permitting absolute paths,
/// traversal, `.git`, or symlink escapes.
pub fn resolve_path(root: &Path, relative: &Path) -> Result<PathBuf> {
    if relative.as_os_str().is_empty() || relative.is_absolute() {
        bail!("path must be relative to the workspace");
    }

    for component in relative.components() {
        match component {
            Component::Normal(value) if !value.to_string_lossy().eq_ignore_ascii_case(".git") => {}
            Component::Normal(_) => bail!("access to .git is not allowed"),
            _ => bail!("path traversal is not allowed: {}", relative.display()),
        }
    }

    let root = root
        .canonicalize()
        .with_context(|| format!("cannot resolve workspace {}", root.display()))?;
    let candidate = root.join(relative);

    let mut ancestor = candidate.as_path();
    while !ancestor.exists() {
        ancestor = ancestor
            .parent()
            .context("path has no existing parent inside the workspace")?;
    }
    let resolved_ancestor = ancestor
        .canonicalize()
        .with_context(|| format!("cannot resolve {}", ancestor.display()))?;
    if !resolved_ancestor.starts_with(&root) {
        bail!("path escapes the workspace through a symlink");
    }

    if candidate.exists() {
        let resolved = candidate
            .canonicalize()
            .with_context(|| format!("cannot resolve {}", candidate.display()))?;
        if !resolved.starts_with(&root) {
            bail!("path escapes the workspace through a symlink");
        }
    }

    Ok(candidate)
}

fn parse_path(raw: &str) -> Result<PathBuf> {
    let path = PathBuf::from(raw.trim());
    if path.as_os_str().is_empty() {
        bail!("file directive has an empty path");
    }
    Ok(path)
}

fn non_empty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_string())
}

fn read_text_if_present(path: &Path) -> Result<Option<String>> {
    match std::fs::read(path) {
        Ok(bytes) => {
            Ok(Some(String::from_utf8(bytes).with_context(|| {
                format!("{} is not UTF-8 text", path.display())
            })?))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("cannot read {}", path.display())),
    }
}

fn apply_hunks(path: &Path, content: &str, hunks: &[Hunk]) -> Result<String> {
    let had_trailing_newline = content.ends_with('\n');
    let mut lines: Vec<String> = content.lines().map(str::to_string).collect();

    for hunk in hunks {
        let old: Vec<&str> = hunk
            .lines
            .iter()
            .filter_map(|line| match line {
                HunkLine::Context(value) | HunkLine::Remove(value) => Some(value.as_str()),
                HunkLine::Add(_) => None,
            })
            .collect();
        let new: Vec<String> = hunk
            .lines
            .iter()
            .filter_map(|line| match line {
                HunkLine::Context(value) | HunkLine::Add(value) => Some(value.clone()),
                HunkLine::Remove(_) => None,
            })
            .collect();

        let marker_positions: Vec<usize> = match hunk.marker.as_deref() {
            Some(marker) => lines
                .iter()
                .enumerate()
                .filter_map(|(index, line)| line.contains(marker).then_some(index))
                .collect(),
            None => Vec::new(),
        };
        if hunk.marker.is_some() && marker_positions.is_empty() {
            bail!(
                "marker {:?} was not found in {}",
                hunk.marker.as_deref().unwrap_or_default(),
                path.display()
            );
        }

        let mut matches = Vec::new();
        if old.is_empty() {
            if marker_positions.len() != 1 {
                bail!(
                    "an insertion-only hunk in {} needs one unique @@ marker",
                    path.display()
                );
            }
            matches.push(marker_positions[0] + 1);
        } else if old.len() <= lines.len() {
            for start in 0..=lines.len() - old.len() {
                if lines[start..start + old.len()]
                    .iter()
                    .map(String::as_str)
                    .eq(old.iter().copied())
                {
                    if marker_positions.is_empty()
                        || marker_positions
                            .iter()
                            .any(|marker| *marker <= start + old.len())
                    {
                        matches.push(start);
                    }
                }
            }
        }

        match matches.as_slice() {
            [] => bail!("hunk context was not found in {}", path.display()),
            [start] => {
                let start = *start;
                let end = start + old.len();
                lines.splice(start..end, new);
            }
            _ => bail!(
                "hunk context is ambiguous in {}; add more context or an @@ marker",
                path.display()
            ),
        }
    }

    let mut result = lines.join("\n");
    if had_trailing_newline {
        result.push('\n');
    }
    Ok(result)
}

/// Open the parent directory one component at a time with `O_NOFOLLOW`.
/// Every later mutation is relative to the returned descriptor, so replacing
/// a checked directory with a symlink cannot redirect a patch outside root.
fn open_parent_beneath(root: &Path, relative: &Path, create: bool) -> Result<(File, CString)> {
    let components = relative
        .components()
        .map(|component| match component {
            Component::Normal(value) => Ok(value),
            _ => bail!("path traversal is not allowed: {}", relative.display()),
        })
        .collect::<Result<Vec<_>>>()?;
    let (filename, directories) = components
        .split_last()
        .context("destination has no filename")?;
    let filename = cstring(filename)?;
    let mut directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW)
        .open(root)
        .with_context(|| format!("cannot open workspace {}", root.display()))?;

    for component in directories {
        let name = cstring(component)?;
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW;
        let mut fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 && create && std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT)
        {
            let rc = unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o755) };
            if rc != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("cannot create {}", relative.display()));
            }
            fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        }
        if fd < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("cannot securely open {}", relative.display()));
        }
        directory = unsafe { File::from_raw_fd(fd) };
    }
    Ok((directory, filename))
}

fn cstring(value: &OsStr) -> Result<CString> {
    CString::new(value.as_bytes()).context("path contains a NUL byte")
}

fn replace_file_beneath(root: &Path, relative: &Path, content: &str) -> Result<()> {
    let (parent, filename) = open_parent_beneath(root, relative, true)?;
    let temporary = CString::new(format!(".gnomef-patch-{}", uuid::Uuid::new_v4()))?;
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC | libc::O_NOFOLLOW;
    let fd = unsafe { libc::openat(parent.as_raw_fd(), temporary.as_ptr(), flags, 0o600) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("cannot create patch temporary for {}", relative.display()));
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    let result = (|| -> Result<()> {
        file.write_all(content.as_bytes())?;
        file.sync_all()?;

        let mut metadata = std::mem::MaybeUninit::<libc::stat>::zeroed();
        let stat_result = unsafe {
            libc::fstatat(
                parent.as_raw_fd(),
                filename.as_ptr(),
                metadata.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if stat_result == 0 {
            let metadata = unsafe { metadata.assume_init() };
            if metadata.st_mode & libc::S_IFMT == libc::S_IFREG {
                unsafe {
                    libc::fchmod(file.as_raw_fd(), metadata.st_mode & 0o7777);
                }
            }
        }

        let rc = unsafe {
            libc::renameat(
                parent.as_raw_fd(),
                temporary.as_ptr(),
                parent.as_raw_fd(),
                filename.as_ptr(),
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("cannot replace {}", relative.display()));
        }
        Ok(())
    })();
    if result.is_err() {
        unsafe {
            libc::unlinkat(parent.as_raw_fd(), temporary.as_ptr(), 0);
        }
    }
    result
}

fn remove_file_beneath(root: &Path, relative: &Path, missing_ok: bool) -> Result<()> {
    let (parent, filename) = match open_parent_beneath(root, relative, false) {
        Ok(value) => value,
        Err(_error) if missing_ok => return Ok(()),
        Err(error) => return Err(error),
    };
    let rc = unsafe { libc::unlinkat(parent.as_raw_fd(), filename.as_ptr(), 0) };
    if rc == 0
        || (missing_ok && std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT))
    {
        return Ok(());
    }
    Err(std::io::Error::last_os_error())
        .with_context(|| format!("cannot delete {}", relative.display()))
}

fn restore_file(root: &Path, relative: &Path, before: Option<&str>) -> Result<()> {
    match before {
        Some(content) => replace_file_beneath(root, relative, content),
        None => remove_file_beneath(root, relative, true),
    }
}

fn restore_file_if_unchanged(
    root: &Path,
    relative: &Path,
    expected_current: Option<&str>,
    before: Option<&str>,
) -> Result<()> {
    let absolute = resolve_path(root, relative)?;
    let current = read_text_if_present(&absolute)?;
    if current.as_deref() != expected_current {
        bail!("file changed after the patch write")
    }
    restore_file(root, relative, before)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace() -> PathBuf {
        let path = std::env::temp_dir().join(format!("gnomef-patch-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn adds_updates_and_deletes() {
        let root = workspace();
        std::fs::write(root.join("old.txt"), "delete me\n").unwrap();
        std::fs::write(root.join("edit.txt"), "one\ntwo\nthree\n").unwrap();

        let patch = parse(
            r#"*** Begin Patch
*** Add File: new.txt
+hello
*** Update File: edit.txt
@@ two
 one
-two
+TWO
 three
*** Delete File: old.txt
*** End Patch"#,
        )
        .unwrap();
        let result = apply(&root, &patch).unwrap();

        assert_eq!(result.changes.len(), 3);
        assert_eq!(
            std::fs::read_to_string(root.join("new.txt")).unwrap(),
            "hello\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("edit.txt")).unwrap(),
            "one\nTWO\nthree\n"
        );
        assert!(!root.join("old.txt").exists());
    }

    #[test]
    fn rejects_parent_traversal() {
        let root = workspace();
        assert!(resolve_path(&root, Path::new("../outside")).is_err());
        assert!(resolve_path(&root, Path::new(".git/config")).is_err());
        assert!(resolve_path(&root, Path::new(".GIT/config")).is_err());
        assert!(resolve_path(&root, Path::new(".Git/config")).is_err());
    }

    #[test]
    fn descriptor_relative_writer_refuses_symlinked_parent() {
        use std::os::unix::fs::symlink;

        let root = workspace();
        let outside = workspace();
        symlink(&outside, root.join("redirect")).unwrap();

        assert!(replace_file_beneath(&root, Path::new("redirect/escaped.txt"), "blocked").is_err());
        assert!(!outside.join("escaped.txt").exists());
    }

    #[test]
    fn descriptor_relative_writer_replaces_final_symlink_not_its_target() {
        use std::os::unix::fs::symlink;

        let root = workspace();
        let outside = workspace().join("outside.txt");
        std::fs::write(&outside, "untouched").unwrap();
        symlink(&outside, root.join("target.txt")).unwrap();

        replace_file_beneath(&root, Path::new("target.txt"), "inside").unwrap();
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "untouched");
        assert_eq!(
            std::fs::read_to_string(root.join("target.txt")).unwrap(),
            "inside"
        );
    }

    #[test]
    fn rejects_oversized_patch_before_parsing() {
        let text = format!(
            "*** Begin Patch\n*** Add File: huge.txt\n+{}\n*** End Patch",
            "x".repeat(MAX_PATCH_BYTES)
        );
        assert!(parse(&text).is_err());
    }

    #[test]
    fn rollback_guard_preserves_concurrent_change() {
        let root = workspace();
        std::fs::write(root.join("changed.txt"), "concurrent\n").unwrap();
        assert!(
            restore_file_if_unchanged(
                &root,
                Path::new("changed.txt"),
                Some("agent\n"),
                Some("before\n")
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(root.join("changed.txt")).unwrap(),
            "concurrent\n"
        );
    }
}
