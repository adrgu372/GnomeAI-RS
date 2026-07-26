use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, anyhow, bail};
use base64::{Engine as _, engine::general_purpose};
use mime_guess::MimeGuess;
use regex::Regex;
use serde::Serialize;
use uuid::Uuid;

use crate::{
    sandbox::{SandboxPolicy, spawn_sandboxed},
    storage::{AppPaths, write_private},
};

pub const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "bmp", "gif", "webp"];

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

pub fn unique_upload_path(directory: &Path, filename: &str) -> anyhow::Result<PathBuf> {
    fs::create_dir_all(directory)?;
    let first = directory.join(filename);
    if !first.exists() {
        return Ok(first);
    }
    let path = Path::new(filename);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("upload");
    let suffix = path.extension().and_then(|s| s.to_str()).unwrap_or("");
    for _ in 0..100 {
        let candidate = if suffix.is_empty() {
            directory.join(format!("{}_{}", stem, Uuid::new_v4().simple()))
        } else {
            directory.join(format!(
                "{}_{}.{}",
                stem,
                &Uuid::new_v4().simple().to_string()[..8],
                suffix
            ))
        };
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    bail!("could not allocate unique upload path")
}

pub fn extension_from_mime(mime: &str, fallback: &str) -> String {
    let clean = mime.split(';').next().unwrap_or("").trim().to_lowercase();
    if clean == "image/jpeg" {
        return ".jpg".into();
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
        .to_lowercase();
    if IMAGE_EXTS.contains(&ext.as_str()) {
        "image".into()
    } else if ext == "pdf" {
        "pdf".into()
    } else if ext == "docx" || ext == "doc" {
        "docx".into()
    } else if ext == "xlsx" || ext == "xls" {
        "excel".into()
    } else {
        "text".into()
    }
}

pub async fn read_file(path: &Path) -> anyhow::Result<ReadFileResult> {
    let file_type = file_type_from_name(path.file_name().and_then(|n| n.to_str()).unwrap_or(""));
    match file_type.as_str() {
        "image" => {
            let ocr = ocr_image(path).await.unwrap_or_default();
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
        _ => {
            let bytes =
                fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
            let content = String::from_utf8(bytes)
                .unwrap_or_else(|err| String::from_utf8_lossy(err.as_bytes()).into_owned());
            Ok(ReadFileResult {
                file_type,
                path: path.to_path_buf(),
                content,
                ocr: String::new(),
            })
        }
    }
}

async fn run_parser(path: &Path, program: &str, args: Vec<String>) -> anyhow::Result<String> {
    let cwd = path
        .parent()
        .ok_or_else(|| anyhow!("uploaded file has no parent directory"))?;
    let mut policy = SandboxPolicy::read_only(cwd);
    policy.allow_network = false;
    policy.require_landlock = true;
    policy.timeout_ms = 30_000;
    policy.max_output_bytes = 1024 * 1024;
    policy.env_allowlist = vec!["PATH".into(), "LANG".into(), "LC_ALL".into()];
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
    let output = run_parser(
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
    )
    .await?;
    if output.is_empty() {
        return Ok(String::new());
    }
    Ok(output)
}

async fn pdftotext(path: &Path) -> anyhow::Result<String> {
    run_parser(
        path,
        "pdftotext",
        vec![path.to_string_lossy().into_owned(), "-".into()],
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

    let fallback = format!(
        "whatsapp-image{}",
        extension_from_mime(mimetype.unwrap_or("image/jpeg"), ".jpg")
    );
    let mut clean = sanitize_upload_filename(filename.unwrap_or(&fallback), &fallback);
    if Path::new(&clean).extension().is_none() {
        clean.push_str(&extension_from_mime(
            mimetype.unwrap_or("image/jpeg"),
            ".jpg",
        ));
    }
    let dest = unique_upload_path(media_dir, &clean)?;
    write_private(&dest, &raw)?;
    let read = read_file(&dest).await?;
    let final_name = dest
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(&clean)
        .to_string();
    paths.ensure_dirs()?;
    Ok((final_name, dest, read))
}
