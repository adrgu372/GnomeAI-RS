use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, anyhow};
use regex::Regex;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    config::AppConfig,
    llama::LlamaClient,
    memory::append_memory_block,
    memory_engine::MemoryEngine,
    sandbox::{SandboxPolicy, spawn_sandboxed},
    storage::{AppPaths, ChatMessage, build_context},
};

pub const DOC_GENERATOR: &str = r#"You are Gnome AI's document generator.

When the user asks you to create a docx, xlsx, or pptx file, respond with exactly
one fenced Python code block that creates the requested file. The runtime injects
OUTPUT_PATH before your code runs. Always save the final file to OUTPUT_PATH.

Available libraries:
- docx / python-docx for Word files
- openpyxl for Excel files
- pptx / python-pptx for PowerPoint files

Rules:
- Generate complete, executable Python.
- Do not read from network resources.
- Do not ask follow-up questions unless the request is impossible.
- Prefer clear structure, useful headings, tables, charts, and formatting.
- Use only code inside the fenced block for the final artifact creation.

Minimal examples:

```python
from docx import Document
doc = Document()
doc.add_heading("Title", level=0)
doc.add_paragraph("Body text")
doc.save(OUTPUT_PATH)
```

```python
from openpyxl import Workbook
wb = Workbook()
ws = wb.active
ws["A1"] = "Title"
wb.save(OUTPUT_PATH)
```

```python
from pptx import Presentation
prs = Presentation()
slide = prs.slides.add_slide(prs.slide_layouts[0])
slide.shapes.title.text = "Title"
prs.save(OUTPUT_PATH)
```
"#;

pub fn detect_format(query: &str) -> Option<&'static str> {
    let q = query.to_lowercase();
    [
        ("docx", "docx"),
        ("word", "docx"),
        ("document", "docx"),
        ("xlsx", "xlsx"),
        ("excel", "xlsx"),
        ("spreadsheet", "xlsx"),
        ("pptx", "pptx"),
        ("powerpoint", "pptx"),
        ("presentation", "pptx"),
        ("prezentare", "pptx"),
        ("document", "docx"),
    ]
    .into_iter()
    .find_map(|(key, format)| q.contains(key).then_some(format))
}

pub async fn generate_document(
    client: &LlamaClient,
    cfg: &AppConfig,
    paths: &AppPaths,
    memory_state: Option<Arc<MemoryEngine>>,
    model: &str,
    query: &str,
    history: &[ChatMessage],
    chat_id: Option<&str>,
    output_format: &str,
) -> anyhow::Result<Value> {
    let example_structure = get_example_structure(history, output_format).await;
    let ctx = build_context(history, cfg.history_window);
    let memory_block = match memory_state {
        Some(engine) => engine
            .working_memory_block(cfg, query, history, chat_id)
            .await
            .ok()
            .filter(|item| !item.trim().is_empty()),
        None => None,
    };
    let mut prompt = format!("Conversation context:\n{ctx}\n\n");
    if !example_structure.trim().is_empty() {
        prompt.push_str(&format!(
            "The user previously uploaded an example .{output_format} file. Replicate its layout, style, and formatting where it fits:\n\n{example_structure}\n\n"
        ));
    }
    prompt.push_str(&format!(
        "User request: {query}\n\nGenerate Python code that creates this file. Write output to OUTPUT_PATH. Do not include explanation outside the code block."
    ));

    let response = client
        .chat(
            cfg,
            model,
            vec![
                json!({
                    "role": "system",
                    "content": append_memory_block(DOC_GENERATOR, memory_block.as_deref())
                }),
                json!({"role": "user", "content": prompt}),
            ],
            0.2,
        )
        .await
        .context("model error")?;
    let full_response = response.content;
    let Some(code) = extract_python_code(&full_response) else {
        return Ok(json!({
            "full_response": full_response,
            "file_url": null,
            "filename": null,
        }));
    };

    let result = execute_generation_code(paths, &code, output_format).await?;
    if result.ok {
        Ok(json!({
            "full_response": full_response,
            "file_url": result.url,
            "filename": result.download_name,
        }))
    } else {
        let err = result.error.chars().rev().take(300).collect::<String>();
        let err = err.chars().rev().collect::<String>();
        Ok(json!({
            "full_response": format!("{full_response}\n\nExecution failed:\n```\n{err}\n```"),
            "file_url": null,
            "filename": null,
        }))
    }
}

fn extract_python_code(text: &str) -> Option<String> {
    Regex::new(r"(?s)```(?:python)?\s*\n(.*?)```")
        .unwrap()
        .captures_iter(text)
        .last()
        .and_then(|caps| caps.get(1).map(|m| m.as_str().trim().to_string()))
}

#[derive(Debug)]
struct GenerationResult {
    ok: bool,
    url: Option<String>,
    download_name: Option<String>,
    error: String,
}

async fn execute_generation_code(
    paths: &AppPaths,
    code: &str,
    output_format: &str,
) -> anyhow::Result<GenerationResult> {
    let gen_id = Uuid::new_v4()
        .simple()
        .to_string()
        .chars()
        .take(12)
        .collect::<String>();
    let tmp_dir = std::env::temp_dir().join(format!("gnome_gen_{gen_id}"));
    fs::create_dir_all(&tmp_dir)?;

    let result = async {
        let output_file = tmp_dir.join(format!("output.{output_format}"));
        let script_path = tmp_dir.join("gen.py");
        let wrapped = format!("OUTPUT_PATH = r'{}'\n\n{code}\n", output_file.display());
        fs::write(&script_path, wrapped)?;

        let mut policy = SandboxPolicy::isolated_workspace_write(&tmp_dir);
        policy.writable = vec![tmp_dir.clone()];
        policy.allow_network = false;
        policy.require_landlock = true;
        policy.timeout_ms = 120_000;
        policy.max_output_bytes = 128 * 1024;
        policy.env_allowlist = vec!["PATH".into(), "LANG".into(), "LC_ALL".into()];
        let output = spawn_sandboxed(
            &policy,
            "python3",
            &[script_path.to_string_lossy().into_owned()],
        )
        .await
        .context("failed to start sandboxed python3")?;
        if output.timed_out {
            return Ok(GenerationResult {
                ok: false,
                url: None,
                download_name: None,
                error: "Code execution timed out (120s limit)".into(),
            });
        }
        if output.cancelled {
            return Ok(GenerationResult {
                ok: false,
                url: None,
                download_name: None,
                error: "Code execution was cancelled".into(),
            });
        }
        if output.exit_code != Some(0) {
            return Ok(GenerationResult {
                ok: false,
                url: None,
                download_name: None,
                error: tail(
                    if output.stderr.trim().is_empty() {
                        &output.stdout
                    } else {
                        &output.stderr
                    },
                    500,
                ),
            });
        }

        let generated = find_generated_file(&tmp_dir, output_format).ok_or_else(|| {
            anyhow!(
                "No .{output_format} file was created.\nstdout: {}\nstderr: {}",
                tail(&output.stdout, 300),
                tail(&output.stderr, 300)
            )
        })?;
        let final_name = format!("gen_{gen_id}.{output_format}");
        let dest = paths.generated_dir.join(&final_name);
        fs::copy(generated, &dest)?;
        fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o600))?;

        Ok(GenerationResult {
            ok: true,
            url: Some(format!("/api/generated/{final_name}")),
            download_name: Some(format!("generated.{output_format}")),
            error: String::new(),
        })
    }
    .await;

    let _ = fs::remove_dir_all(&tmp_dir);
    result
}

fn find_generated_file(tmp_dir: &Path, output_format: &str) -> Option<PathBuf> {
    let mut fallback = None;
    for entry in fs::read_dir(tmp_dir).ok()? {
        let path = entry.ok()?.path();
        if !path.is_file() {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if name == "gen.py" || name.starts_with('.') {
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) == Some(output_format) {
            return Some(path);
        }
        fallback.get_or_insert(path);
    }
    fallback
}

fn tail(text: &str, max_chars: usize) -> String {
    let chars = text.chars().collect::<Vec<_>>();
    let start = chars.len().saturating_sub(max_chars);
    chars[start..].iter().collect()
}

async fn get_example_structure(history: &[ChatMessage], output_format: &str) -> String {
    let Some((target_ext, script)) = (match output_format {
        "docx" => Some(("docx", DOCX_STRUCTURE_SCRIPT)),
        "xlsx" => Some(("xlsx", XLSX_STRUCTURE_SCRIPT)),
        "pptx" => Some(("pptx", PPTX_STRUCTURE_SCRIPT)),
        _ => None,
    }) else {
        return String::new();
    };

    for message in history {
        let Value::Object(obj) = &message.content else {
            continue;
        };
        let filename = obj.get("filename").and_then(Value::as_str).unwrap_or("");
        let path = obj.get("path").and_then(Value::as_str).unwrap_or("");
        if !filename.ends_with(&format!(".{target_ext}")) || path.is_empty() {
            continue;
        }
        let path = Path::new(path);
        if !path.exists() {
            continue;
        }
        return run_structure_script(script, path).await;
    }
    String::new()
}

async fn run_structure_script(script: &str, path: &Path) -> String {
    let Some(cwd) = path.parent() else {
        return "Could not parse example: invalid path".into();
    };
    let mut policy = SandboxPolicy::read_only(cwd);
    policy.allow_network = false;
    policy.require_landlock = true;
    policy.timeout_ms = 30_000;
    policy.max_output_bytes = 16 * 1024;
    policy.env_allowlist = vec!["PATH".into(), "LANG".into(), "LC_ALL".into()];
    let args = vec![
        "-c".into(),
        script.into(),
        path.to_string_lossy().into_owned(),
    ];
    match spawn_sandboxed(&policy, "python3", &args).await {
        Ok(output) if output.exit_code == Some(0) && !output.timed_out => {
            output.stdout.trim().chars().take(8_000).collect()
        }
        Ok(output) if output.timed_out => "Could not parse example: timed out".into(),
        Ok(output) => format!(
            "Could not parse example: {}",
            tail(
                if output.stderr.trim().is_empty() {
                    &output.stdout
                } else {
                    &output.stderr
                },
                500
            )
        ),
        Err(err) => format!("Could not parse example: {err}"),
    }
}

const DOCX_STRUCTURE_SCRIPT: &str = r##"
import sys
from pathlib import Path
try:
    from docx import Document
    doc = Document(sys.argv[1])
    lines = [f"=== Example DOCX: {Path(sys.argv[1]).name} ==="]
    sec = doc.sections[0]
    lines.append(f"Page: {sec.page_width.inches:.1f}x{sec.page_height.inches:.1f}in | Margins: T={sec.top_margin.inches:.1f} B={sec.bottom_margin.inches:.1f} L={sec.left_margin.inches:.1f} R={sec.right_margin.inches:.1f}")
    for p in doc.paragraphs[:80]:
        if not p.text.strip():
            continue
        sname = p.style.name if p.style else "Normal"
        align = str(p.alignment) if p.alignment else "LEFT"
        fmt_parts = []
        for run in p.runs[:3]:
            parts = []
            if run.bold:
                parts.append("B")
            if run.italic:
                parts.append("I")
            if run.font.size:
                parts.append(f"{run.font.size.pt:.0f}pt")
            if run.font.color and run.font.color.rgb:
                parts.append(f"#{run.font.color.rgb}")
            if parts:
                fmt_parts.append(",".join(parts))
        fmt = f" [{'; '.join(fmt_parts)}]" if fmt_parts else ""
        lines.append(f"  [{sname}|{align}]{fmt} {p.text[:120]}")
    for i, table in enumerate(doc.tables[:10]):
        lines.append(f"  TABLE {i+1}: {len(table.rows)}R x {len(table.columns)}C")
        for r, row in enumerate(table.rows[:5]):
            cells = [cell.text[:40] for cell in row.cells]
            lines.append(f"    Row {r}: {' | '.join(cells)}")
    print("\n".join(lines))
except Exception as exc:
    print(f"Could not parse DOCX: {exc}")
"##;

const XLSX_STRUCTURE_SCRIPT: &str = r##"
import sys
from pathlib import Path
try:
    from openpyxl import load_workbook
    wb = load_workbook(sys.argv[1], read_only=True, data_only=True)
    lines = [f"=== Example XLSX: {Path(sys.argv[1]).name} ==="]
    for ws in wb.worksheets[:5]:
        lines.append(f"\nSheet: '{ws.title}' ({ws.max_row}R x {ws.max_column}C)")
        rows_data = list(ws.iter_rows(max_row=min(15, ws.max_row), values_only=True))
        if rows_data:
            headers = [str(c) if c else f"Col{i+1}" for i, c in enumerate(rows_data[0])]
            lines.append(f"  Headers: {' | '.join(headers[:10])}")
            for r, row in enumerate(rows_data[1:6], 2):
                vals = [str(c)[:25] if c else "" for c in row[:10]]
                lines.append(f"  Row {r}: {' | '.join(vals)}")
    wb.close()
    print("\n".join(lines))
except Exception as exc:
    print(f"Could not parse XLSX: {exc}")
"##;

const PPTX_STRUCTURE_SCRIPT: &str = r##"
import sys
from pathlib import Path
try:
    from pptx import Presentation
    prs = Presentation(sys.argv[1])
    lines = [f"=== Example PPTX: {Path(sys.argv[1]).name} ===", f"Slides: {len(prs.slides)}"]
    for i, slide in enumerate(prs.slides[:20]):
        layout = slide.slide_layout.name
        shapes_info = []
        for shape in slide.shapes:
            stype = shape.shape_type
            name = stype.name if hasattr(stype, "name") else str(stype)
            if shape.has_text_frame:
                text = shape.text_frame.text[:80].replace("\n", " ")
                shapes_info.append(f"Text({name}): '{text}'")
            elif shape.has_table:
                table = shape.table
                shapes_info.append(f"Table({len(table.rows)}x{len(table.columns)})")
            else:
                shapes_info.append(name)
        info = " | ".join(shapes_info[:6]) if shapes_info else "(empty)"
        lines.append(f"  Slide {i+1} [{layout}]: {info}")
    print("\n".join(lines))
except Exception as exc:
    print(f"Could not parse PPTX: {exc}")
"##;
