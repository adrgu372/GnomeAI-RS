use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, anyhow, bail};
use base64::{Engine as _, engine::general_purpose};
use mime_guess::MimeGuess;
use regex::Regex;
use serde::Serialize;
use tracing::warn;
use uuid::Uuid;

use crate::{
    config::AppConfig,
    sandbox::{SandboxPolicy, spawn_sandboxed},
    storage::AppPaths,
    transcribe::resolve_transcription_provider,
};

pub const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "bmp", "gif", "webp"];

/// Audio containers a Whisper-style endpoint accepts. WhatsApp voice notes
/// arrive as `ogg`/opus, forwarded files as anything in this list.
const AUDIO_EXTS: &[&str] = &[
    "ogg", "oga", "opus", "mp3", "mpga", "m4a", "aac", "wav", "flac", "amr", "wma", "weba",
];

/// Video containers. The transcription endpoint demuxes these server-side,
/// so no local ffmpeg is required.
const VIDEO_EXTS: &[&str] = &[
    "mp4", "m4v", "mov", "webm", "mkv", "avi", "3gp", "mpeg", "mpg",
];

/// OOXML/ODF containers: ZIP archives whose text lives in XML parts.
pub const OOXML_EXTS: &[&str] = &[
    "docx", "xlsx", "pptx", "docm", "xlsm", "pptm", "odt", "ods", "odp",
];

/// Extensions read as plain text. Source code is the bulk of it — the list
/// exists so a known-good extension skips the binary heuristic below.
pub const TEXT_EXTS: &[&str] = &[
    // documents and data
    "txt",
    "md",
    "markdown",
    "rst",
    "log",
    "csv",
    "tsv",
    "json",
    "jsonl",
    "yaml",
    "yml",
    "toml",
    "ini",
    "cfg",
    "conf",
    "env",
    "properties",
    "xml",
    "html",
    "htm",
    "svg",
    "tex",
    "org",
    // code
    "rs",
    "py",
    "pyi",
    "js",
    "mjs",
    "cjs",
    "jsx",
    "ts",
    "tsx",
    "go",
    "java",
    "kt",
    "kts",
    "c",
    "h",
    "cc",
    "cpp",
    "cxx",
    "hpp",
    "hh",
    "cs",
    "swift",
    "m",
    "mm",
    "rb",
    "php",
    "pl",
    "pm",
    "lua",
    "sh",
    "bash",
    "zsh",
    "fish",
    "ps1",
    "bat",
    "cmd",
    "sql",
    "r",
    "jl",
    "scala",
    "clj",
    "cljs",
    "ex",
    "exs",
    "erl",
    "hrl",
    "hs",
    "ml",
    "mli",
    "fs",
    "fsx",
    "dart",
    "zig",
    "nim",
    "v",
    "vhd",
    "vhdl",
    "asm",
    "s",
    "f90",
    "f95",
    "pas",
    "groovy",
    "gradle",
    "tf",
    "tfvars",
    "hcl",
    "proto",
    "graphql",
    "gql",
    "vue",
    "svelte",
    "css",
    "scss",
    "sass",
    "less",
    "styl",
    "dockerfile",
    "makefile",
    "mk",
    "cmake",
    "patch",
    "diff",
    "gitignore",
    "editorconfig",
    "lock",
];

/// Maximum text extracted from one container, and the archive limits that
/// keep a malicious "zip bomb" from exhausting memory.
const MAX_EXTRACTED_CHARS: usize = 400_000;
const MAX_ARCHIVE_ENTRIES: usize = 2_000;
const MAX_ARCHIVE_BYTES: u64 = 80 * 1024 * 1024;
pub const MAX_UPLOAD_STORAGE_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
pub struct ReadFileResult {
    pub file_type: String,
    pub path: PathBuf,
    pub content: String,
    pub ocr: String,
}

pub fn sanitize_upload_filename(filename: &str, fallback: &str) -> String {
    let name = Path::new(filename)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(fallback)
        .trim();
    let re = Regex::new(r"[^A-Za-z0-9._ -]+").unwrap();
    let cleaned = re.replace_all(name, "_");
    let cleaned = cleaned.trim_matches([' ', '.']).to_string();
    if cleaned.is_empty() {
        fallback.to_string()
    } else {
        cleaned.chars().take(180).collect()
    }
}

fn upload_candidates(directory: &Path, filename: &str) -> Vec<PathBuf> {
    let mut candidates = vec![directory.join(filename)];
    let path = Path::new(filename);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("upload");
    let suffix = path.extension().and_then(|s| s.to_str()).unwrap_or("");
    for _ in 0..100 {
        candidates.push(if suffix.is_empty() {
            directory.join(format!("{}_{}", stem, Uuid::new_v4().simple()))
        } else {
            directory.join(format!(
                "{}_{}.{}",
                stem,
                &Uuid::new_v4().simple().to_string()[..8],
                suffix
            ))
        });
    }
    candidates
}

/// Create and write the final upload through one `O_NOFOLLOW|O_EXCL` file
/// descriptor. This closes the existence-check/symlink race from the old
/// `unique_upload_path` + `fs::write` sequence.
pub fn write_unique_private(
    directory: &Path,
    filename: &str,
    data: &[u8],
) -> anyhow::Result<PathBuf> {
    fs::create_dir_all(directory)?;
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
    for candidate in upload_candidates(directory, filename) {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&candidate);
        let mut file = match file {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        };
        let result = file.write_all(data).and_then(|_| file.sync_all());
        if let Err(error) = result {
            drop(file);
            let _ = fs::remove_file(&candidate);
            return Err(error.into());
        }
        return Ok(candidate);
    }
    bail!("could not allocate unique upload path")
}

pub fn ensure_upload_capacity(root: &Path, incoming_bytes: usize) -> anyhow::Result<()> {
    let used = directory_bytes(root)?;
    let incoming = u64::try_from(incoming_bytes).unwrap_or(u64::MAX);
    if used.saturating_add(incoming) > MAX_UPLOAD_STORAGE_BYTES {
        bail!(
            "upload storage quota exceeded ({} MiB used, 1024 MiB limit)",
            used / (1024 * 1024)
        )
    }
    Ok(())
}

fn directory_bytes(root: &Path) -> anyhow::Result<u64> {
    if !root.exists() {
        return Ok(0);
    }
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        for entry in fs::read_dir(directory)? {
            let entry = entry?;
            let metadata = fs::symlink_metadata(entry.path())?;
            if metadata.file_type().is_symlink() {
                continue;
            }
            if metadata.is_dir() {
                stack.push(entry.path());
            } else if metadata.is_file() {
                total = total.saturating_add(metadata.len());
            }
        }
    }
    Ok(total)
}

pub fn extension_from_mime(mime: &str, fallback: &str) -> String {
    let clean = mime.split(';').next().unwrap_or("").trim().to_lowercase();
    // Pin the common types: `mime_guess` returns extensions in an arbitrary
    // order, so `text/plain` would otherwise become `.asm`.
    const PREFERRED: &[(&str, &str)] = &[
        ("image/jpeg", ".jpg"),
        ("image/jpg", ".jpg"),
        ("image/png", ".png"),
        ("image/webp", ".webp"),
        ("image/gif", ".gif"),
        ("image/bmp", ".bmp"),
        ("application/pdf", ".pdf"),
        (
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
            ".docx",
        ),
        (
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            ".xlsx",
        ),
        (
            "application/vnd.openxmlformats-officedocument.presentationml.presentation",
            ".pptx",
        ),
        ("application/msword", ".doc"),
        ("application/vnd.ms-excel", ".xls"),
        ("application/vnd.ms-powerpoint", ".ppt"),
        ("application/vnd.oasis.opendocument.text", ".odt"),
        ("application/vnd.oasis.opendocument.spreadsheet", ".ods"),
        ("application/vnd.oasis.opendocument.presentation", ".odp"),
        ("application/rtf", ".rtf"),
        ("application/json", ".json"),
        ("application/xml", ".xml"),
        ("application/zip", ".zip"),
        ("text/plain", ".txt"),
        ("text/markdown", ".md"),
        ("text/csv", ".csv"),
        ("text/html", ".html"),
        ("text/xml", ".xml"),
        ("text/css", ".css"),
        ("text/javascript", ".js"),
        ("text/x-python", ".py"),
        ("text/x-rust", ".rs"),
        ("text/x-c", ".c"),
        ("text/x-csrc", ".c"),
        ("text/x-c++src", ".cpp"),
        ("text/x-java", ".java"),
        ("text/x-shellscript", ".sh"),
        ("audio/ogg", ".ogg"),
        ("audio/opus", ".opus"),
        ("audio/mpeg", ".mp3"),
        ("audio/mp4", ".m4a"),
        ("audio/aac", ".aac"),
        ("audio/wav", ".wav"),
        ("audio/x-wav", ".wav"),
        ("audio/flac", ".flac"),
        ("audio/amr", ".amr"),
        ("video/mp4", ".mp4"),
        ("video/quicktime", ".mov"),
        ("video/webm", ".webm"),
        ("video/x-matroska", ".mkv"),
        ("video/3gpp", ".3gp"),
    ];
    if let Some((_, ext)) = PREFERRED.iter().find(|(name, _)| *name == clean) {
        return (*ext).to_string();
    }
    if clean.starts_with("text/") {
        return ".txt".into();
    }
    if clean.starts_with("audio/") {
        return ".ogg".into();
    }
    if clean.starts_with("video/") {
        return ".mp4".into();
    }
    mime_guess::get_mime_extensions_str(&clean)
        .and_then(|items| items.first().copied())
        .map(|ext| format!(".{ext}"))
        .unwrap_or_else(|| fallback.to_string())
}

pub fn file_type_from_name(filename: &str) -> String {
    let ext = Path::new(filename)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        // Extensionless names like `Makefile` or `Dockerfile` are still code.
        .to_lowercase();
    let stem = Path::new(filename)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("")
        .to_lowercase();
    if IMAGE_EXTS.contains(&ext.as_str()) {
        "image".into()
    } else if AUDIO_EXTS.contains(&ext.as_str()) {
        "audio".into()
    } else if VIDEO_EXTS.contains(&ext.as_str()) {
        "video".into()
    } else if ext == "pdf" {
        "pdf".into()
    } else if OOXML_EXTS.contains(&ext.as_str()) {
        match ext.as_str() {
            "xlsx" | "xlsm" | "ods" => "excel".into(),
            "pptx" | "pptm" | "odp" => "slides".into(),
            _ => "docx".into(),
        }
    } else if ext == "doc" || ext == "xls" || ext == "ppt" || ext == "rtf" {
        // Legacy binary Office formats have no in-process extractor.
        "binary".into()
    } else if TEXT_EXTS.contains(&ext.as_str())
        || (ext.is_empty() && TEXT_EXTS.contains(&stem.as_str()))
    {
        "text".into()
    } else {
        // Unknown extension: decided by content sniffing in `read_file`.
        "text".into()
    }
}

/// True when the bytes look like a binary blob rather than readable text.
/// A NUL byte, or a high share of control characters, is the giveaway.
fn looks_binary(bytes: &[u8]) -> bool {
    let sample = &bytes[..bytes.len().min(8_000)];
    if sample.is_empty() {
        return false;
    }
    if sample.contains(&0) {
        return true;
    }
    let control = sample
        .iter()
        .filter(|byte| **byte < 0x09 || (**byte > 0x0d && **byte < 0x20))
        .count();
    control * 100 / sample.len() > 5
}

/// Text of an OOXML/ODF document: unzip in memory, keep the XML parts that
/// carry content, and strip the tags. Deliberately in-process — it needs no
/// external converter and cannot execute anything from the archive.
fn ooxml_text(path: &Path) -> anyhow::Result<String> {
    let file =
        fs::File::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    let mut archive = zip::ZipArchive::new(std::io::BufReader::new(file))
        .with_context(|| format!("{} is not a valid OOXML/ODF container", path.display()))?;
    if archive.len() > MAX_ARCHIVE_ENTRIES {
        bail!("archive has too many entries");
    }

    // Shared strings must be read before sheets so xlsx cells resolve.
    let mut names = (0..archive.len())
        .filter_map(|index| {
            archive
                .by_index_raw(index)
                .ok()
                .map(|item| item.name().to_string())
        })
        .filter(|name| is_content_part(name))
        .collect::<Vec<_>>();
    names.sort_by_key(|name| (!name.contains("sharedStrings"), name.clone()));

    let mut total = 0_u64;
    let mut out = String::new();
    for name in names {
        let entry = match archive.by_name(&name) {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        total += entry.size();
        if total > MAX_ARCHIVE_BYTES {
            bail!("archive expands beyond the size limit");
        }
        let mut raw = String::new();
        use std::io::Read as _;
        if entry
            .take(MAX_ARCHIVE_BYTES)
            .read_to_string(&mut raw)
            .is_err()
        {
            continue;
        }
        let text = strip_xml_tags(&raw);
        if text.trim().is_empty() {
            continue;
        }
        out.push_str(text.trim());
        out.push('\n');
        if out.len() >= MAX_EXTRACTED_CHARS {
            out.truncate(MAX_EXTRACTED_CHARS);
            break;
        }
    }
    Ok(out.trim().to_string())
}

/// Extract readable text from a document selected in the native GUI.
///
/// The GUI runs outside the async request path, so this deliberately uses a
/// synchronous `pdftotext` process for PDFs. Office containers and source
/// files stay in-process and share the same size and binary checks as uploads.
pub fn extract_text_attachment(path: &Path) -> anyhow::Result<String> {
    let file_type = file_type_from_name(
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(""),
    );
    match file_type.as_str() {
        "pdf" => {
            let output = Command::new("pdftotext")
                .arg(path)
                .arg("-")
                .output()
                .with_context(|| "PDF support requires the `poppler-utils` package")?;
            if !output.status.success() {
                let detail = String::from_utf8_lossy(&output.stderr);
                bail!("cannot extract PDF text: {}", detail.trim());
            }
            Ok(String::from_utf8_lossy(&output.stdout)
                .chars()
                .take(MAX_EXTRACTED_CHARS)
                .collect())
        }
        "docx" | "excel" | "slides" => ooxml_text(path),
        "binary" => {
            bail!("legacy binary Office files are unsupported; use PDF, DOCX, XLSX, PPTX, or text")
        }
        "image" | "audio" | "video" => {
            bail!("this attachment type does not contain directly readable document text")
        }
        _ => {
            let bytes =
                fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
            if looks_binary(&bytes) {
                bail!("the selected file appears to be binary")
            }
            Ok(String::from_utf8(bytes)
                .unwrap_or_else(|error| String::from_utf8_lossy(error.as_bytes()).into_owned())
                .chars()
                .take(MAX_EXTRACTED_CHARS)
                .collect())
        }
    }
}

/// The archive members that actually hold document text.
fn is_content_part(name: &str) -> bool {
    if !name.ends_with(".xml") && name != "content.xml" {
        return false;
    }
    // OpenDocument.
    if name == "content.xml" {
        return true;
    }
    // OOXML: body, slides and notes, worksheets plus the shared string table.
    name == "word/document.xml"
        || name.starts_with("word/footnotes")
        || name.starts_with("word/endnotes")
        || name.starts_with("ppt/slides/slide")
        || name.starts_with("ppt/notesSlides/notesSlide")
        || name.starts_with("xl/worksheets/sheet")
        || name == "xl/sharedStrings.xml"
}

/// Minimal XML-to-text: paragraph-ish tags become newlines, cell and run
/// boundaries become spaces, everything else is dropped and entities are
/// decoded. Good enough to feed a model, with no XML dependency.
fn strip_xml_tags(xml: &str) -> String {
    let mut out = String::with_capacity(xml.len() / 4);
    let mut chars = xml.char_indices().peekable();
    while let Some((index, ch)) = chars.next() {
        if ch != '<' {
            out.push(ch);
            continue;
        }
        let Some(end) = xml[index..].find('>').map(|offset| index + offset) else {
            break;
        };
        let tag = &xml[index + 1..end];
        let name = tag
            .trim_start_matches('/')
            .split([' ', '/', '\t', '\n'])
            .next()
            .unwrap_or("");
        if matches!(
            name,
            "w:p"
                | "a:p"
                | "text:p"
                | "text:h"
                | "row"
                | "table:table-row"
                | "w:br"
                | "w:tab"
                // One shared-string entry per spreadsheet cell value.
                | "si"
        ) {
            out.push('\n');
        } else if matches!(
            name,
            "c" | "w:tc" | "table:table-cell" | "w:r" | "a:r" | "t" | "w:t" | "a:t"
        ) {
            out.push(' ');
        }
        while let Some((next, _)) = chars.peek() {
            if *next > end {
                break;
            }
            chars.next();
        }
    }
    let decoded = out
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#10;", "\n")
        .replace("&#13;", "\n");
    // Collapse the whitespace the tag stripping leaves behind.
    decoded
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

pub async fn read_file(path: &Path) -> anyhow::Result<ReadFileResult> {
    let file_type = file_type_from_name(path.file_name().and_then(|n| n.to_str()).unwrap_or(""));
    match file_type.as_str() {
        "image" => {
            // OCR is best-effort, but a silent failure means images quietly
            // lose their text — log the reason instead of swallowing it.
            let ocr = match ocr_image(path).await {
                Ok(text) => text,
                Err(error) => {
                    warn!("OCR failed for {}: {error:#}", path.display());
                    String::new()
                }
            };
            Ok(ReadFileResult {
                file_type,
                path: path.to_path_buf(),
                content: String::new(),
                ocr,
            })
        }
        "pdf" => {
            let content = pdftotext(path)
                .await
                .unwrap_or_else(|_| "PDF text extraction unavailable.".into());
            Ok(ReadFileResult {
                file_type,
                path: path.to_path_buf(),
                content,
                ocr: String::new(),
            })
        }
        "docx" | "excel" | "slides" => {
            let content = ooxml_text(path)
                .unwrap_or_else(|error| format!("Document text extraction unavailable: {error}"));
            Ok(ReadFileResult {
                file_type,
                path: path.to_path_buf(),
                content,
                ocr: String::new(),
            })
        }
        // Speech needs the configured transcription endpoint, which
        // `read_file` has no access to; `transcribe_media` fills these in.
        "audio" | "video" => Ok(ReadFileResult {
            file_type,
            path: path.to_path_buf(),
            content: String::new(),
            ocr: String::new(),
        }),
        "binary" => Ok(ReadFileResult {
            file_type,
            path: path.to_path_buf(),
            content: "This legacy binary format is not supported. Please resend it as PDF, \
                      DOCX, XLSX, PPTX, or plain text."
                .into(),
            ocr: String::new(),
        }),
        _ => {
            let bytes =
                fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
            if looks_binary(&bytes) {
                return Ok(ReadFileResult {
                    file_type: "binary".into(),
                    path: path.to_path_buf(),
                    content: "This file appears to be binary, so there is no text to read.".into(),
                    ocr: String::new(),
                });
            }
            let content = String::from_utf8(bytes)
                .unwrap_or_else(|err| String::from_utf8_lossy(err.as_bytes()).into_owned())
                .chars()
                .take(MAX_EXTRACTED_CHARS)
                .collect();
            Ok(ReadFileResult {
                file_type,
                path: path.to_path_buf(),
                content,
                ocr: String::new(),
            })
        }
    }
}

/// Text of an attachment, transcribing audio and video on the way. Every
/// other type already carries its text from [`read_file`].
///
/// Returns a plain-language note instead of an error when transcription is
/// unavailable: the file is stored either way, and the model should be told
/// what arrived rather than seeing an empty message.
pub async fn transcribe_media(
    read: &ReadFileResult,
    cfg: &AppConfig,
    http: &reqwest::Client,
) -> String {
    if read.file_type != "audio" && read.file_type != "video" {
        return if read.content.is_empty() {
            read.ocr.clone()
        } else {
            read.content.clone()
        };
    }
    let kind = if read.file_type == "audio" {
        "Recording"
    } else {
        "Video"
    };
    let Some(provider) = resolve_transcription_provider(cfg, http) else {
        return format!(
            "[{kind} received. Speech-to-text is not configured, so its spoken content is \
             unavailable — set `transcription_model` to transcribe it.]"
        );
    };
    match provider.transcribe(&read.path).await {
        Ok(text) if !text.trim().is_empty() => text,
        Ok(_) => format!("[{kind} received; the transcription came back empty (no speech).]"),
        Err(error) => {
            warn!("transcription failed for {}: {error}", read.path.display());
            format!("[{kind} received, but transcription failed: {error}]")
        }
    }
}

async fn run_parser_with_env(
    path: &Path,
    program: &str,
    args: Vec<String>,
    env_extra: Vec<(String, String)>,
) -> anyhow::Result<String> {
    let cwd = path
        .parent()
        .ok_or_else(|| anyhow!("uploaded file has no parent directory"))?;
    let mut policy = SandboxPolicy::read_only(cwd);
    policy.allow_network = false;
    policy.require_landlock = true;
    policy.timeout_ms = 30_000;
    policy.max_output_bytes = 1024 * 1024;
    policy.env_allowlist = vec!["PATH".into(), "LANG".into(), "LC_ALL".into()];
    policy.env_extra = env_extra;
    let output = spawn_sandboxed(&policy, program, &args)
        .await
        .with_context(|| format!("failed to run sandboxed {program}"))?;
    if output.timed_out {
        bail!("{program} timed out after 30 seconds");
    }
    if output.exit_code != Some(0) {
        let detail = if output.stderr.trim().is_empty() {
            output.stdout.trim()
        } else {
            output.stderr.trim()
        };
        bail!("{program} failed: {detail}");
    }
    Ok(output.stdout.trim().to_string())
}

async fn ocr_image(path: &Path) -> anyhow::Result<String> {
    let output = run_parser_with_env(
        path,
        "tesseract",
        vec![
            path.to_string_lossy().into_owned(),
            "stdout".into(),
            "--oem".into(),
            "1".into(),
            "--psm".into(),
            "3".into(),
        ],
        // Tesseract parallelises with OpenMP, but `RLIMIT_NPROC` counts every
        // process the user already runs, so thread creation fails on a busy
        // desktop and OCR silently returns nothing. One thread is also faster
        // for single images.
        vec![("OMP_THREAD_LIMIT".into(), "1".into())],
    )
    .await?;
    if output.is_empty() {
        return Ok(String::new());
    }
    Ok(output)
}

async fn pdftotext(path: &Path) -> anyhow::Result<String> {
    run_parser_with_env(
        path,
        "pdftotext",
        vec![path.to_string_lossy().into_owned(), "-".into()],
        Vec::new(),
    )
    .await
}

pub fn encode_image_as_base64(path: &Path) -> anyhow::Result<String> {
    let bytes = fs::read(path)?;
    Ok(general_purpose::STANDARD.encode(bytes))
}

pub fn encode_image_as_data_url(path: &Path) -> anyhow::Result<String> {
    let mime = MimeGuess::from_path(path).first_or_octet_stream();
    Ok(format!(
        "data:{mime};base64,{}",
        encode_image_as_base64(path)?
    ))
}

pub async fn save_base64_image(
    paths: &AppPaths,
    media_dir: &Path,
    filename: Option<&str>,
    mimetype: Option<&str>,
    b64: &str,
) -> anyhow::Result<(String, PathBuf, ReadFileResult)> {
    save_base64_media(paths, media_dir, filename, mimetype, b64, true).await
}

/// Persist inbound base64 media (WhatsApp attachments) and extract its text.
/// `image_defaults` only decides the fallback name/extension when the sender
/// supplied neither; the actual handling follows the resulting file type.
pub async fn save_base64_media(
    paths: &AppPaths,
    media_dir: &Path,
    filename: Option<&str>,
    mimetype: Option<&str>,
    b64: &str,
    image_defaults: bool,
) -> anyhow::Result<(String, PathBuf, ReadFileResult)> {
    let raw_b64 = b64
        .split_once(',')
        .filter(|(prefix, _)| prefix.starts_with("data:"))
        .map(|(_, body)| body)
        .unwrap_or(b64)
        .split_whitespace()
        .collect::<String>();
    let raw = general_purpose::STANDARD.decode(raw_b64)?;
    if raw.len() > 50 * 1024 * 1024 {
        bail!("file too large");
    }
    ensure_upload_capacity(&paths.uploads_dir, raw.len())?;

    let (default_mime, default_stem, default_ext) = if image_defaults {
        ("image/jpeg", "whatsapp-image", ".jpg")
    } else {
        ("application/octet-stream", "whatsapp-file", ".bin")
    };
    let mime = mimetype.unwrap_or(default_mime);
    let fallback = format!("{default_stem}{}", extension_from_mime(mime, default_ext));
    let mut clean = sanitize_upload_filename(filename.unwrap_or(&fallback), &fallback);
    if Path::new(&clean).extension().is_none() {
        clean.push_str(&extension_from_mime(mime, default_ext));
    }
    let dest = write_unique_private(media_dir, &clean, &raw)?;
    let read = read_file(&dest).await?;
    let final_name = dest
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&clean)
        .to_string();
    paths.ensure_dirs()?;
    Ok((final_name, dest, read))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gnomef-uploads-{}", Uuid::new_v4().simple()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Build a minimal but structurally real OOXML package.
    fn write_ooxml(path: &Path, parts: &[(&str, &str)]) {
        let file = fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        for (name, body) in parts {
            zip.start_file(*name, options).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }

    #[test]
    fn file_types_cover_documents_and_code() {
        assert_eq!(file_type_from_name("photo.JPG"), "image");
        assert_eq!(file_type_from_name("report.pdf"), "pdf");
        assert_eq!(file_type_from_name("notes.docx"), "docx");
        assert_eq!(file_type_from_name("budget.xlsx"), "excel");
        assert_eq!(file_type_from_name("deck.pptx"), "slides");
        assert_eq!(file_type_from_name("notes.odt"), "docx");
        assert_eq!(file_type_from_name("legacy.doc"), "binary");
        for name in [
            "main.rs",
            "app.py",
            "index.ts",
            "Main.java",
            "query.sql",
            "script.sh",
            "notes.txt",
            "data.csv",
            "config.yaml",
            "Makefile",
            "Dockerfile",
            "style.scss",
        ] {
            assert_eq!(file_type_from_name(name), "text", "{name} should be text");
        }
    }

    #[tokio::test]
    async fn docx_text_is_extracted() {
        let dir = temp_dir();
        let path = dir.join("note.docx");
        write_ooxml(
            &path,
            &[(
                "word/document.xml",
                r#"<?xml version="1.0"?><w:document><w:body>
                   <w:p><w:r><w:t>Raport lunar</w:t></w:r></w:p>
                   <w:p><w:r><w:t>Total &amp; final: 42</w:t></w:r></w:p>
                   </w:body></w:document>"#,
            )],
        );
        let read = read_file(&path).await.unwrap();
        assert_eq!(read.file_type, "docx");
        assert!(read.content.contains("Raport lunar"), "{}", read.content);
        assert!(
            read.content.contains("Total & final: 42"),
            "{}",
            read.content
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn xlsx_and_pptx_text_is_extracted() {
        let dir = temp_dir();
        let xlsx = dir.join("sheet.xlsx");
        write_ooxml(
            &xlsx,
            &[
                (
                    "xl/sharedStrings.xml",
                    r#"<sst><si><t>Produs</t></si><si><t>Cantitate</t></si></sst>"#,
                ),
                (
                    "xl/worksheets/sheet1.xml",
                    r#"<worksheet><sheetData><row><c><v>17</v></c></row></sheetData></worksheet>"#,
                ),
            ],
        );
        let read = read_file(&xlsx).await.unwrap();
        assert_eq!(read.file_type, "excel");
        assert!(read.content.contains("Produs"), "{}", read.content);
        assert!(read.content.contains("17"), "{}", read.content);

        let pptx = dir.join("deck.pptx");
        write_ooxml(
            &pptx,
            &[(
                "ppt/slides/slide1.xml",
                r#"<p:sld><p:cSld><a:p><a:r><a:t>Titlu prezentare</a:t></a:r></a:p></p:cSld></p:sld>"#,
            )],
        );
        let read = read_file(&pptx).await.unwrap();
        assert_eq!(read.file_type, "slides");
        assert!(
            read.content.contains("Titlu prezentare"),
            "{}",
            read.content
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn code_and_text_files_are_read_verbatim() {
        let dir = temp_dir();
        let path = dir.join("main.rs");
        fs::write(&path, "fn main() {\n    println!(\"salut\");\n}\n").unwrap();
        let read = read_file(&path).await.unwrap();
        assert_eq!(read.file_type, "text");
        assert!(read.content.contains("println!(\"salut\")"));
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn binary_files_do_not_leak_garbage_text() {
        let dir = temp_dir();
        let path = dir.join("mystery.dat");
        fs::write(&path, [0x00, 0x01, 0x02, 0xff, 0xfe, 0x00, 0x7f]).unwrap();
        let read = read_file(&path).await.unwrap();
        assert_eq!(read.file_type, "binary");
        assert!(read.content.contains("binary"));
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn whatsapp_document_is_saved_with_its_own_name_and_text() {
        let dir = temp_dir();
        let paths = AppPaths::new(dir.join("state")).unwrap();
        let media_dir = paths.whatsapp_media_dir.clone();

        let source = dir.join("raport.docx");
        write_ooxml(
            &source,
            &[(
                "word/document.xml",
                r#"<w:document><w:body><w:p><w:r><w:t>Continut din WhatsApp</w:t></w:r></w:p></w:body></w:document>"#,
            )],
        );
        let b64 = general_purpose::STANDARD.encode(fs::read(&source).unwrap());

        let (name, path, read) = save_base64_media(
            &paths,
            &media_dir,
            Some("raport.docx"),
            Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
            &b64,
            false,
        )
        .await
        .unwrap();
        assert_eq!(name, "raport.docx");
        assert!(path.exists());
        assert_eq!(read.file_type, "docx");
        assert!(read.content.contains("Continut din WhatsApp"));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn audio_video_and_sticker_types_are_recognized() {
        for name in [
            "nota.ogg",
            "voce.opus",
            "melodie.mp3",
            "clip.m4a",
            "rec.wav",
            "voce.amr",
        ] {
            assert_eq!(file_type_from_name(name), "audio", "{name}");
        }
        for name in ["clip.mp4", "film.mov", "clip.webm", "clip.3gp", "clip.mkv"] {
            assert_eq!(file_type_from_name(name), "video", "{name}");
        }
        // Stickers are WebP: they must stay on the image path.
        assert_eq!(file_type_from_name("sticker.webp"), "image");
    }

    #[tokio::test]
    async fn audio_carries_no_text_until_transcribed() {
        let dir = temp_dir();
        let path = dir.join("nota.ogg");
        fs::write(&path, b"OggS not-real-audio").unwrap();
        let read = read_file(&path).await.unwrap();
        assert_eq!(read.file_type, "audio");
        assert!(read.content.is_empty());
        assert!(read.ocr.is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn media_without_transcription_configured_explains_itself() {
        let dir = temp_dir();
        let http = reqwest::Client::new();
        let cfg = AppConfig::default();
        assert!(cfg.transcription_model.is_empty());

        for (name, kind) in [("nota.ogg", "Recording"), ("clip.mp4", "Video")] {
            let path = dir.join(name);
            fs::write(&path, b"placeholder").unwrap();
            let read = read_file(&path).await.unwrap();
            let text = transcribe_media(&read, &cfg, &http).await;
            assert!(text.contains(kind), "{name}: {text}");
            assert!(
                text.contains("not configured"),
                "the note must say why there is no transcript: {text}"
            );
        }
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn transcribe_media_passes_other_types_through() {
        let dir = temp_dir();
        let http = reqwest::Client::new();
        let cfg = AppConfig::default();

        let text_path = dir.join("note.txt");
        fs::write(&text_path, "continut simplu").unwrap();
        let read = read_file(&text_path).await.unwrap();
        assert_eq!(
            transcribe_media(&read, &cfg, &http).await,
            "continut simplu"
        );

        // Images keep using their OCR field.
        let image = ReadFileResult {
            file_type: "image".into(),
            path: dir.join("poza.png"),
            content: String::new(),
            ocr: "text din imagine".into(),
        };
        assert_eq!(
            transcribe_media(&image, &cfg, &http).await,
            "text din imagine"
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn whatsapp_voice_note_is_stored_as_audio() {
        let dir = temp_dir();
        let paths = AppPaths::new(dir.join("state")).unwrap();
        let media_dir = paths.whatsapp_media_dir.clone();
        let b64 = general_purpose::STANDARD.encode(b"OggS fake voice note payload");

        let (name, path, read) =
            save_base64_media(&paths, &media_dir, None, Some("audio/ogg"), &b64, false)
                .await
                .unwrap();
        assert!(name.ends_with(".ogg"), "unexpected name: {name}");
        assert!(path.exists());
        assert_eq!(read.file_type, "audio");
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn whatsapp_attachment_without_a_name_keeps_its_mime_extension() {
        let dir = temp_dir();
        let paths = AppPaths::new(dir.join("state")).unwrap();
        let media_dir = paths.whatsapp_media_dir.clone();
        let b64 = general_purpose::STANDARD.encode(b"salut, acesta este un fisier text\n");

        let (name, _, read) =
            save_base64_media(&paths, &media_dir, None, Some("text/plain"), &b64, false)
                .await
                .unwrap();
        assert!(name.ends_with(".txt"), "unexpected name: {name}");
        assert_eq!(read.file_type, "text");
        assert!(read.content.contains("acesta este un fisier text"));
        let _ = fs::remove_dir_all(dir);
    }
}
