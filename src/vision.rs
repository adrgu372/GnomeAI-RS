use std::path::PathBuf;

use regex::Regex;
use serde_json::{Value, json};

use crate::{
    config::AppConfig,
    consistency::enforce_final_answer,
    llama::{LlamaClient, ModelInfo},
    memory::append_memory_block,
    runtime_profile::{RuntimeProfile, build_runtime_aware_system_prompt},
    storage::ChatMessage,
    uploads::{encode_image_as_data_url, file_type_from_name},
};

pub const SYSTEM_PROMPT: &str = r#"You are Gnome AI, a practical local assistant.
Answer in the user's language. Be direct and useful.
If the user uploaded an image and it is attached, analyze the visual content.
If only OCR text is available, say that visual details are limited to OCR."#;

const IMAGE_INTENT_TERMS: &[&str] = &[
    "image",
    "picture",
    "photo",
    "screenshot",
    "describe",
    "analyze",
    "what is this",
    "look at",
    "interpret",
    "explain this",
    "read this",
    "summarize this",
    "what do you see",
    "imagine",
    "poza",
    "foto",
    "fotografie",
    "captura",
    "ecran",
    "analizeaza",
    "descrie",
    "uita-te",
    "uitate",
    "priveste",
    "interpreteaza",
    "citeste",
    "extrage",
    "ocr",
    "ce vezi",
    "ce este",
    "ce e",
    "ce-i",
    "ce scrie",
    "ce apare",
    "ce contine",
    "imaginea asta",
    "poza asta",
    "atasata",
    "atasat",
];

pub fn normalize_intent_text(text: &str) -> String {
    text.to_lowercase()
        .replace(['ă', 'â'], "a")
        .replace('î', "i")
        .replace(['ș', 'ş'], "s")
        .replace(['ț', 'ţ'], "t")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn is_image_intent(query: &str) -> bool {
    let q = normalize_intent_text(query);
    IMAGE_INTENT_TERMS.iter().any(|term| q.contains(term))
}

pub fn supports_images(model: &str, known_models: &[ModelInfo]) -> bool {
    if model.trim().is_empty() {
        return false;
    }

    let re = Regex::new(
        r"(?i)(vision|omni|vl|vila|llava|bakllava|minicpm|moondream|internvl|qwen.*(?:vl|vision)|pixtral|gemma[-_.: ]?3|gemma3|llama[-_.: ]?3\.2.*vision|mistral.*vision|ministral)",
    )
    .unwrap();
    if re.is_match(model) {
        return true;
    }
    let Some(info) = known_models.iter().find(|info| info.id == model) else {
        // Model catalogues are frequently incomplete or lag behind newly
        // released multimodal models. Let the provider make the authoritative
        // decision instead of blocking the attachment in the client.
        return true;
    };

    let capabilities = info
        .capabilities
        .iter()
        .map(|capability| capability.trim().to_ascii_lowercase())
        .collect::<Vec<_>>();
    if capabilities.iter().any(|capability| {
        matches!(
            capability.as_str(),
            "vision" | "image" | "images" | "multimodal" | "input_image" | "image_input"
        )
    }) {
        return true;
    }

    // Only an explicit negative marker is restrictive. A catalogue that says
    // merely `text` may simply omit modality metadata, so it remains eligible.
    !capabilities.iter().any(|capability| {
        matches!(
            capability.as_str(),
            "text-only"
                | "text_only"
                | "no-image"
                | "no_image"
                | "no-vision"
                | "no_vision"
                | "vision-disabled"
        )
    })
}

pub fn build_image_vision_messages(
    system_prompt: &str,
    query: &str,
    image_name: &str,
    image_path: &PathBuf,
) -> anyhow::Result<Vec<Value>> {
    let prompt = format!(
        "The user uploaded an image called '{image_name}'. The image is attached to this message. Analyze the actual visual content in detail and answer in the same language as the user. Do not claim you cannot see the image unless the attachment is missing. User's question: {query}"
    );
    Ok(vec![
        json!({"role": "system", "content": system_prompt}),
        json!({
            "role": "user",
            "content": [
                {"type": "text", "text": prompt},
                {"type": "image_url", "image_url": {"url": encode_image_as_data_url(image_path)?}}
            ]
        }),
    ])
}

pub fn find_extracted_content(
    history: &[ChatMessage],
    query: &str,
) -> (Option<String>, Option<String>, Option<String>) {
    for (index, msg) in history.iter().enumerate().rev() {
        if let Some(obj) = msg.content.as_object() {
            if obj.get("type").and_then(Value::as_str) == Some("image") {
                if should_use_image_upload(history, index, query) {
                    let ocr = obj
                        .get("ocr")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let filename = obj
                        .get("filename")
                        .and_then(Value::as_str)
                        .unwrap_or("image")
                        .to_string();
                    return (Some(ocr), Some("image".into()), Some(filename));
                }
                continue;
            }
        }

        let Some(text) = msg.content.as_str() else {
            continue;
        };
        if !text.starts_with("[Extracted content from uploaded file:") {
            continue;
        }
        let Some((header, body)) = text.split_once("]\n\n") else {
            continue;
        };
        let filename = header
            .trim_start_matches("[Extracted content from uploaded file:")
            .trim()
            .to_string();
        let file_type = file_type_from_name(&filename);
        if file_type == "image" && !should_use_image_upload(history, index, query) {
            continue;
        }
        return (
            Some(body.to_string()),
            Some(file_type),
            Some(if filename.is_empty() {
                "file".into()
            } else {
                filename
            }),
        );
    }
    (None, None, None)
}

pub fn find_image_path(
    history: &[ChatMessage],
    filename: Option<&str>,
) -> (Option<PathBuf>, String) {
    for msg in history.iter().rev() {
        let Some(obj) = msg.content.as_object() else {
            continue;
        };
        if obj.get("type").and_then(Value::as_str) != Some("image") {
            continue;
        }
        let name = obj
            .get("filename")
            .and_then(Value::as_str)
            .unwrap_or("image");
        if filename.is_some_and(|wanted| wanted != name) {
            continue;
        }
        if let Some(path) = obj.get("path").and_then(Value::as_str) {
            let path = PathBuf::from(path);
            if path.exists() {
                return (Some(path), name.to_string());
            }
        }
    }
    (None, String::new())
}

pub async fn generate_image_response(
    client: &LlamaClient,
    cfg: &AppConfig,
    runtime_profile: &RuntimeProfile,
    memory_block: Option<&str>,
    model: &str,
    known_models: &[ModelInfo],
    query: &str,
    history: &[ChatMessage],
    extracted: Option<String>,
    filename: Option<String>,
) -> String {
    let (image_path, image_name_from_history) = find_image_path(history, filename.as_deref());
    let image_name = if !image_name_from_history.is_empty() {
        image_name_from_history
    } else {
        filename.unwrap_or_else(|| "image".into())
    };
    let system_prompt = append_memory_block(
        &build_runtime_aware_system_prompt(SYSTEM_PROMPT, runtime_profile),
        memory_block,
    );

    if let Some(path) = image_path.as_ref() {
        if supports_images(model, known_models) {
            match build_image_vision_messages(&system_prompt, query, &image_name, path) {
                Ok(messages) => match client.chat(cfg, model, messages, 0.2).await {
                    Ok(response) if !response.content.trim().is_empty() => {
                        return enforce_final_answer(&response.content, runtime_profile, &[]);
                    }
                    Ok(_) => {}
                    Err(err) => {
                        let ocr = extracted.unwrap_or_default();
                        if !ocr.trim().is_empty() {
                            return answer_from_ocr(
                                client,
                                cfg,
                                runtime_profile,
                                memory_block,
                                model,
                                query,
                                &image_name,
                                &ocr,
                                Some(&err.to_string()),
                            )
                            .await;
                        }
                        return format!(
                            "The image was received, but the selected provider or model rejected the visual request. For a local model, also check that the projector/mmproj is loaded. Error: {err}"
                        );
                    }
                },
                Err(err) => {
                    return format!("The image could not be encoded for the model: {err}");
                }
            }
        }
    }

    let ocr = extracted.unwrap_or_default();
    if !ocr.trim().is_empty() {
        return answer_from_ocr(
            client,
            cfg,
            runtime_profile,
            memory_block,
            model,
            query,
            &image_name,
            &ocr,
            None,
        )
        .await;
    }

    if image_path.is_some() {
        format!(
            "The image '{image_name}' was received, but the selected model '{model}' is explicitly marked as text-only and OCR found no text. Select a multimodal model for visual analysis."
        )
    } else {
        "The attached image was not found in the chat history.".into()
    }
}

fn should_use_image_upload(history: &[ChatMessage], index: usize, query: &str) -> bool {
    is_image_intent(query)
        || !history[index + 1..]
            .iter()
            .any(|msg| matches!(msg.role.as_str(), "assistant" | "gnome"))
}

async fn answer_from_ocr(
    client: &LlamaClient,
    cfg: &AppConfig,
    runtime_profile: &RuntimeProfile,
    memory_block: Option<&str>,
    model: &str,
    query: &str,
    image_name: &str,
    ocr: &str,
    vision_error: Option<&str>,
) -> String {
    let note = if let Some(err) = vision_error {
        format!("The vision request failed, so I can answer only from OCR. Error: {err}\n\n")
    } else {
        format!(
            "Model '{model}' is explicitly marked as text-only, so I can answer only from OCR.\n\n"
        )
    };
    let prompt = format!(
        "{note}Image: {image_name}\nUser question: {query}\n\nOCR text:\n{ocr}"
    );
    let system_prompt = append_memory_block(
        &build_runtime_aware_system_prompt(SYSTEM_PROMPT, runtime_profile),
        memory_block,
    );
    let messages = vec![
        json!({"role": "system", "content": system_prompt}),
        json!({"role": "user", "content": prompt}),
    ];
    match client.chat(cfg, model, messages, 0.3).await {
        Ok(response) if !response.content.trim().is_empty() => {
            enforce_final_answer(&response.content, runtime_profile, &[])
        }
        Ok(_) => "[Empty response]".into(),
        Err(_) => prompt,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_omni_models_as_image_capable() {
        assert!(supports_images(
            "nvidia/nemotron-3-nano-omni-30b-a3b-reasoning:free",
            &[],
        ));
    }

    #[test]
    fn uses_provider_image_modality_metadata() {
        let known_models = vec![ModelInfo {
            id: "provider/model-with-an-uninformative-name".into(),
            capabilities: vec!["text".into(), "image".into()],
        }];
        assert!(supports_images(
            "provider/model-with-an-uninformative-name",
            &known_models,
        ));
    }

    #[test]
    fn unknown_models_are_optimistically_allowed_to_try_images() {
        assert!(supports_images("provider/new-multimodal-model", &[]));

        let incomplete_metadata = vec![ModelInfo {
            id: "provider/catalogue-only-says-text".into(),
            capabilities: vec!["text".into()],
        }];
        assert!(supports_images(
            "provider/catalogue-only-says-text",
            &incomplete_metadata,
        ));
    }

    #[test]
    fn only_explicit_text_only_metadata_blocks_image_input() {
        let known_models = vec![ModelInfo {
            id: "provider/text-only-model".into(),
            capabilities: vec!["text-only".into()],
        }];
        assert!(!supports_images("provider/text-only-model", &known_models,));
        assert!(!supports_images("", &known_models));
    }
}
