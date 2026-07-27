//! Speech-to-text for inbound voice notes, audio files and video.
//!
//! Native Rust, same shape as [`crate::embeddings`]: a trait with one
//! OpenAI-compatible implementation (`/v1/audio/transcriptions`, which every
//! Whisper-style server exposes — whisper.cpp's HTTP server, faster-whisper,
//! LocalAI, vLLM, OpenAI itself). No Python, no bundled model, no external
//! binary. When nothing is configured the caller keeps the file and says
//! plainly that it could not be transcribed.
//!
//! Video needs no demuxer: the transcription endpoint accepts the container
//! itself (`mp4`, `webm`, `mpeg`) and pulls the audio track out server-side.

use std::{path::Path, sync::Arc, time::Duration};

use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;
use serde_json::Value;

use crate::config::AppConfig;

/// Transcription is slow compared to a chat completion; a voice note of a few
/// minutes must not be cut off.
const TRANSCRIBE_TIMEOUT: Duration = Duration::from_secs(180);
/// Beyond this the endpoint would reject the upload anyway.
pub const MAX_TRANSCRIBE_BYTES: u64 = 25 * 1024 * 1024;

#[async_trait]
pub trait TranscriptionProvider: Send + Sync {
    /// Identity for diagnostics, e.g. `openai:whisper-1`.
    fn id(&self) -> String;

    /// Transcribe one media file. Returns the plain text, which may be empty
    /// when the recording holds no speech.
    async fn transcribe(&self, path: &Path) -> anyhow::Result<String>;
}

/// OpenAI-compatible `/v1/audio/transcriptions`.
pub struct OpenAiTranscription {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    language: String,
}

#[async_trait]
impl TranscriptionProvider for OpenAiTranscription {
    fn id(&self) -> String {
        format!("openai:{}", self.model)
    }

    async fn transcribe(&self, path: &Path) -> anyhow::Result<String> {
        let metadata =
            std::fs::metadata(path).with_context(|| format!("cannot stat {}", path.display()))?;
        if metadata.len() > MAX_TRANSCRIBE_BYTES {
            bail!(
                "recording is {} MB, above the {} MB transcription limit",
                metadata.len() / (1024 * 1024),
                MAX_TRANSCRIBE_BYTES / (1024 * 1024)
            );
        }
        let bytes = tokio::fs::read(path)
            .await
            .with_context(|| format!("cannot read {}", path.display()))?;
        let filename = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("audio")
            .to_string();

        let mut last_error = None;
        for url in candidate_urls(&self.base_url) {
            let mut form = reqwest::multipart::Form::new()
                .text("model", self.model.clone())
                .text("response_format", "json")
                .part(
                    "file",
                    reqwest::multipart::Part::bytes(bytes.clone()).file_name(filename.clone()),
                );
            // A language hint measurably improves accuracy for Romanian.
            if !self.language.is_empty() {
                form = form.text("language", self.language.clone());
            }

            let mut request = self
                .http
                .post(&url)
                .timeout(TRANSCRIBE_TIMEOUT)
                .multipart(form);
            if !self.api_key.is_empty() {
                request = request.bearer_auth(&self.api_key);
            }
            let response = match request.send().await {
                Ok(response) => response,
                Err(err) => {
                    last_error = Some(anyhow!("{url}: {err}"));
                    continue;
                }
            };
            if response.status().as_u16() == 404 {
                continue;
            }
            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                last_error = Some(anyhow!(
                    "{url} returned {status}: {}",
                    body.chars().take(300).collect::<String>()
                ));
                continue;
            }
            let raw = response.text().await.context("empty transcription body")?;
            return Ok(parse_transcription(&raw));
        }
        Err(last_error.unwrap_or_else(|| anyhow!("no transcription endpoint answered")))
    }
}

/// Backend for the current configuration, or `None` when transcription is
/// off or unconfigured.
pub fn resolve_transcription_provider(
    cfg: &AppConfig,
    http: &reqwest::Client,
) -> Option<Arc<dyn TranscriptionProvider>> {
    let model = cfg.transcription_model.trim();
    if cfg.transcription_provider.trim() == "off" || model.is_empty() {
        return None;
    }
    let base = cfg.transcription_base_url.trim().trim_end_matches('/');
    Some(Arc::new(OpenAiTranscription {
        http: http.clone(),
        base_url: if base.is_empty() {
            cfg.llama_base_url.trim().trim_end_matches('/').to_string()
        } else {
            base.to_string()
        },
        // Reuses the provider key from the config file; it is never copied
        // into stored state.
        api_key: cfg.llama_api_key.trim().to_string(),
        model: model.to_string(),
        language: cfg.transcription_language.trim().to_string(),
    }))
}

fn candidate_urls(base: &str) -> Vec<String> {
    let base = base.trim_end_matches('/');
    let mut urls = vec![format!("{base}/audio/transcriptions")];
    if !base.ends_with("/v1") {
        urls.push(format!("{base}/v1/audio/transcriptions"));
    }
    urls
}

/// Servers answer with `{"text": "..."}`; some return bare text despite the
/// requested format, so accept both.
fn parse_transcription(raw: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(raw) {
        if let Some(text) = value.get("text").and_then(Value::as_str) {
            return text.trim().to_string();
        }
        // Verbose format: concatenate the segments.
        if let Some(segments) = value.get("segments").and_then(Value::as_array) {
            let joined = segments
                .iter()
                .filter_map(|segment| segment.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(" ");
            if !joined.trim().is_empty() {
                return joined.split_whitespace().collect::<Vec<_>>().join(" ");
            }
        }
    }
    raw.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_follows_configuration() {
        let http = reqwest::Client::new();
        let mut cfg = AppConfig::default();
        assert!(resolve_transcription_provider(&cfg, &http).is_none());

        cfg.transcription_model = "whisper-1".into();
        let provider = resolve_transcription_provider(&cfg, &http).unwrap();
        assert_eq!(provider.id(), "openai:whisper-1");

        cfg.transcription_provider = "off".into();
        assert!(resolve_transcription_provider(&cfg, &http).is_none());
    }

    #[test]
    fn transcription_bodies_are_parsed() {
        assert_eq!(
            parse_transcription(r#"{"text":"  Salut, ce mai faci?  "}"#),
            "Salut, ce mai faci?"
        );
        assert_eq!(
            parse_transcription(
                r#"{"segments":[{"text":"Prima parte"},{"text":" a doua parte"}]}"#
            ),
            "Prima parte a doua parte"
        );
        assert_eq!(parse_transcription("text simplu\n"), "text simplu");
    }

    #[test]
    fn endpoint_candidates_cover_both_base_url_shapes() {
        assert_eq!(
            candidate_urls("http://127.0.0.1:8090/v1"),
            vec!["http://127.0.0.1:8090/v1/audio/transcriptions"]
        );
        assert_eq!(
            candidate_urls("http://127.0.0.1:8090"),
            vec![
                "http://127.0.0.1:8090/audio/transcriptions",
                "http://127.0.0.1:8090/v1/audio/transcriptions",
            ]
        );
    }
}
