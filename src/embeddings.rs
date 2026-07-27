//! Embedding backends for the semantic memory engine.
//!
//! Two HTTP providers (OpenAI-compatible `/v1/embeddings` and Ollama
//! `/api/embed`) behind one trait, plus the vector math the engine needs.
//! There is deliberately no vector server: at the configured cap of 5 000
//! facts an exact scan is a few hundred microseconds, far below the cost of
//! the HTTP round-trip that produced the query vector.
//!
//! API keys are read from the live [`AppConfig`] on every call and are never
//! persisted next to the vectors.

use std::{sync::Arc, time::Duration};

use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;
use serde_json::{Value, json};

use crate::config::AppConfig;

pub const MIN_EMBEDDING_DIM: usize = 8;
pub const MAX_EMBEDDING_DIM: usize = 8_192;
const EMBED_TIMEOUT: Duration = Duration::from_secs(20);
/// Providers reject unbounded batches; the engine chunks to this size.
pub const EMBED_BATCH: usize = 32;

#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Stable identity persisted next to each stored vector, e.g.
    /// `openai:text-embedding-3-small`. Retrieval only compares vectors whose
    /// identity matches the active provider; a change of identity is what
    /// triggers reindexing.
    fn id(&self) -> String;

    /// Embed a batch of texts. Implementations must return one vector per
    /// input, already validated by [`validate_embeddings`].
    async fn embed(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>>;
}

/// OpenAI-compatible `/v1/embeddings` endpoint (llama.cpp, vLLM, OpenAI, …).
pub struct OpenAiEmbeddings {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
}

#[async_trait]
impl EmbeddingProvider for OpenAiEmbeddings {
    fn id(&self) -> String {
        format!("openai:{}", self.model)
    }

    async fn embed(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        let mut last_error = None;
        for url in candidate_openai_urls(&self.base_url) {
            let mut request = self
                .http
                .post(&url)
                .timeout(EMBED_TIMEOUT)
                .json(&json!({"model": self.model, "input": texts}));
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
            let payload: Value = response.json().await.context("invalid embeddings JSON")?;
            let vectors = parse_openai_embeddings(&payload)?;
            validate_embeddings(texts.len(), &vectors)?;
            return Ok(vectors);
        }
        Err(last_error.unwrap_or_else(|| anyhow!("no embeddings endpoint answered")))
    }
}

/// Ollama `/api/embed` endpoint.
pub struct OllamaEmbeddings {
    http: reqwest::Client,
    base_url: String,
    model: String,
}

#[async_trait]
impl EmbeddingProvider for OllamaEmbeddings {
    fn id(&self) -> String {
        format!("ollama:{}", self.model)
    }

    async fn embed(&self, texts: &[String]) -> anyhow::Result<Vec<Vec<f32>>> {
        let url = format!("{}/api/embed", self.base_url.trim_end_matches('/'));
        let response = self
            .http
            .post(&url)
            .timeout(EMBED_TIMEOUT)
            .json(&json!({"model": self.model, "input": texts}))
            .send()
            .await
            .with_context(|| format!("cannot reach {url}"))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            bail!(
                "{url} returned {status}: {}",
                body.chars().take(300).collect::<String>()
            );
        }
        let payload: Value = response.json().await.context("invalid embeddings JSON")?;
        let vectors = payload
            .get("embeddings")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("Ollama response has no `embeddings` array"))?
            .iter()
            .map(parse_vector)
            .collect::<anyhow::Result<Vec<_>>>()?;
        validate_embeddings(texts.len(), &vectors)?;
        Ok(vectors)
    }
}

/// Pick the embedding backend for the current configuration. `None` means
/// embeddings are not configured (or explicitly off) and the engine falls
/// back to lexical retrieval.
pub fn resolve_embedding_provider(
    cfg: &AppConfig,
    http: &reqwest::Client,
) -> Option<Arc<dyn EmbeddingProvider>> {
    let model = cfg.embeddings_model.trim();
    let kind = cfg.embeddings_provider.trim();
    if kind == "off" || model.is_empty() {
        return None;
    }
    let base = cfg.embeddings_base_url.trim().trim_end_matches('/');
    let use_ollama = kind == "ollama" || (kind != "openai" && base.contains(":11434"));
    if use_ollama {
        Some(Arc::new(OllamaEmbeddings {
            http: http.clone(),
            base_url: if base.is_empty() {
                "http://127.0.0.1:11434".into()
            } else {
                base.to_string()
            },
            model: model.to_string(),
        }))
    } else {
        Some(Arc::new(OpenAiEmbeddings {
            http: http.clone(),
            base_url: if base.is_empty() {
                cfg.llama_base_url.trim().trim_end_matches('/').to_string()
            } else {
                base.to_string()
            },
            // The provider key is reused for the embeddings endpoint; it lives
            // only in the config file, never in the memory database.
            api_key: cfg.llama_api_key.trim().to_string(),
            model: model.to_string(),
        }))
    }
}

fn candidate_openai_urls(base: &str) -> Vec<String> {
    let base = base.trim_end_matches('/');
    let mut urls = vec![format!("{base}/embeddings")];
    if !base.ends_with("/v1") {
        urls.push(format!("{base}/v1/embeddings"));
    }
    urls
}

fn parse_openai_embeddings(payload: &Value) -> anyhow::Result<Vec<Vec<f32>>> {
    payload
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("embeddings response has no `data` array"))?
        .iter()
        .map(|item| {
            item.get("embedding")
                .ok_or_else(|| anyhow!("embeddings item has no `embedding`"))
                .and_then(parse_vector)
        })
        .collect()
}

fn parse_vector(value: &Value) -> anyhow::Result<Vec<f32>> {
    value
        .as_array()
        .ok_or_else(|| anyhow!("embedding is not an array"))?
        .iter()
        .map(|item| {
            item.as_f64()
                .map(|number| number as f32)
                .ok_or_else(|| anyhow!("embedding contains a non-numeric value"))
        })
        .collect()
}

/// Reject malformed provider output before it can reach the database:
/// wrong batch size, inconsistent or out-of-range dimensions, non-finite
/// values, or an all-zero vector.
pub fn validate_embeddings(expected: usize, vectors: &[Vec<f32>]) -> anyhow::Result<()> {
    if vectors.len() != expected {
        bail!(
            "provider returned {} vectors for {} inputs",
            vectors.len(),
            expected
        );
    }
    let dim = vectors.first().map(Vec::len).unwrap_or(0);
    for vector in vectors {
        validate_embedding(vector)?;
        if vector.len() != dim {
            bail!(
                "inconsistent embedding dimensions ({} vs {dim})",
                vector.len()
            );
        }
    }
    Ok(())
}

pub fn validate_embedding(vector: &[f32]) -> anyhow::Result<()> {
    if !(MIN_EMBEDDING_DIM..=MAX_EMBEDDING_DIM).contains(&vector.len()) {
        bail!(
            "embedding dimension {} outside {MIN_EMBEDDING_DIM}..={MAX_EMBEDDING_DIM}",
            vector.len()
        );
    }
    if vector.iter().any(|value| !value.is_finite()) {
        bail!("embedding contains a non-finite value");
    }
    if vector.iter().all(|value| *value == 0.0) {
        bail!("embedding is all zeros");
    }
    Ok(())
}

/// Cosine similarity in `[-1, 1]`; `0.0` for mismatched or degenerate input.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0_f64;
    let mut norm_a = 0.0_f64;
    let mut norm_b = 0.0_f64;
    for (x, y) in a.iter().zip(b) {
        dot += f64::from(*x) * f64::from(*y);
        norm_a += f64::from(*x) * f64::from(*x);
        norm_b += f64::from(*y) * f64::from(*y);
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    (dot / (norm_a.sqrt() * norm_b.sqrt())) as f32
}

/// Little-endian `f32` packing for the SQLite BLOB column.
pub fn encode_embedding(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(vector.len() * 4);
    for value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

pub fn decode_embedding(bytes: &[u8]) -> Option<Vec<f32>> {
    if bytes.is_empty() || bytes.len() % 4 != 0 {
        return None;
    }
    Some(
        bytes
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_similarity_matches_expectations() {
        let a = [1.0, 0.0, 0.0];
        let b = [1.0, 0.0, 0.0];
        let c = [0.0, 1.0, 0.0];
        let d = [-1.0, 0.0, 0.0];
        assert!((cosine_similarity(&a, &b) - 1.0).abs() < 1e-6);
        assert!(cosine_similarity(&a, &c).abs() < 1e-6);
        assert!((cosine_similarity(&a, &d) + 1.0).abs() < 1e-6);
        assert_eq!(cosine_similarity(&a, &[1.0]), 0.0);
        assert_eq!(cosine_similarity(&[], &[]), 0.0);
    }

    #[test]
    fn embedding_blob_round_trips() {
        let vector = vec![0.25_f32, -1.5, 3.75, 0.0];
        let decoded = decode_embedding(&encode_embedding(&vector)).unwrap();
        assert_eq!(decoded, vector);
        assert!(decode_embedding(&[1, 2, 3]).is_none());
        assert!(decode_embedding(&[]).is_none());
    }

    #[test]
    fn validation_rejects_bad_vectors() {
        assert!(validate_embedding(&vec![0.5; 16]).is_ok());
        assert!(validate_embedding(&vec![0.5; 2]).is_err());
        assert!(validate_embedding(&vec![0.5; MAX_EMBEDDING_DIM + 1]).is_err());
        assert!(validate_embedding(&[f32::NAN; 16]).is_err());
        assert!(validate_embedding(&[0.0; 16]).is_err());
        assert!(validate_embeddings(2, &[vec![0.5; 16]]).is_err());
        assert!(validate_embeddings(2, &[vec![0.5; 16], vec![0.5; 8]]).is_err());
        assert!(validate_embeddings(2, &[vec![0.5; 16], vec![0.25; 16]]).is_ok());
    }

    #[test]
    fn provider_resolution_follows_configuration() {
        let http = reqwest::Client::new();
        let mut cfg = AppConfig::default();
        assert!(resolve_embedding_provider(&cfg, &http).is_none());

        cfg.embeddings_model = "nomic-embed-text".into();
        let provider = resolve_embedding_provider(&cfg, &http).unwrap();
        assert_eq!(provider.id(), "openai:nomic-embed-text");

        cfg.embeddings_base_url = "http://127.0.0.1:11434".into();
        let provider = resolve_embedding_provider(&cfg, &http).unwrap();
        assert_eq!(provider.id(), "ollama:nomic-embed-text");

        cfg.embeddings_provider = "openai".into();
        let provider = resolve_embedding_provider(&cfg, &http).unwrap();
        assert_eq!(provider.id(), "openai:nomic-embed-text");

        cfg.embeddings_provider = "off".into();
        assert!(resolve_embedding_provider(&cfg, &http).is_none());
    }
}
