//! OpenRouter-specific recovery helpers.
//!
//! Paid-credit fallback is intentionally discovered at request time: free
//! model availability and rankings move too quickly for a hardcoded catalog.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Context, bail};
use reqwest::StatusCode;
use serde_json::Value;

const MODELS_URL: &str = "https://openrouter.ai/api/v1/models";
const FREE_ROUTER: &str = "openrouter/free";
const MAX_SPECIFIC_FALLBACKS: usize = 8;

pub fn is_credit_exhausted(status: StatusCode) -> bool {
    status == StatusCode::PAYMENT_REQUIRED
}

pub fn free_router_only() -> Vec<String> {
    vec![FREE_ROUTER.to_string()]
}

/// Ask OpenRouter for the best currently listed free coding/agent models.
/// The server performs the benchmark sort; local filtering then guarantees
/// zero text/request cost and the capabilities required by this request.
pub async fn ranked_free_models(
    client: &reqwest::Client,
    api_key: Option<&str>,
    needs_tools: bool,
    needs_images: bool,
) -> anyhow::Result<Vec<String>> {
    let sort = if needs_tools {
        "agentic-high-to-low"
    } else {
        "intelligence-high-to-low"
    };
    let mut query = vec![
        ("category", "programming"),
        ("sort", sort),
        ("max_price", "0"),
        ("max_output_price", "0"),
        ("limit", "50"),
    ];
    if needs_tools {
        query.push(("supported_parameters", "tools"));
    }
    if needs_images {
        query.push(("input_modalities", "text,image"));
    }

    let mut request = client
        .get(MODELS_URL)
        .query(&query)
        .timeout(Duration::from_secs(8));
    if let Some(api_key) = api_key.map(str::trim).filter(|key| !key.is_empty()) {
        request = request.bearer_auth(api_key);
    }
    let response = request
        .send()
        .await
        .context("failed to load OpenRouter free models")?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        bail!(
            "OpenRouter models endpoint returned {status}: {}",
            body.chars().take(400).collect::<String>()
        );
    }
    let payload: Value = response
        .json()
        .await
        .context("invalid OpenRouter models response")?;
    Ok(ranked_free_models_from_payload(
        &payload,
        needs_tools,
        needs_images,
    ))
}

fn ranked_free_models_from_payload(
    payload: &Value,
    needs_tools: bool,
    needs_images: bool,
) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut models = payload
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|model| model_is_free(model, needs_images))
        .filter(|model| !needs_tools || supports(model, "/supported_parameters", "tools"))
        .filter(|model| !needs_images || supports(model, "/architecture/input_modalities", "image"))
        .filter_map(|model| model.get("id").and_then(Value::as_str))
        .map(str::trim)
        .filter(|id| !id.is_empty() && *id != FREE_ROUTER)
        .filter(|id| seen.insert((*id).to_string()))
        .take(MAX_SPECIFIC_FALLBACKS)
        .map(str::to_string)
        .collect::<Vec<_>>();

    // If every ranked candidate is temporarily unavailable, OpenRouter's
    // free router remains a final capability-aware escape hatch.
    models.push(FREE_ROUTER.to_string());
    models
}

fn model_is_free(model: &Value, needs_images: bool) -> bool {
    let Some(pricing) = model.get("pricing") else {
        return false;
    };
    price_is_zero(pricing.get("prompt"), false)
        && price_is_zero(pricing.get("completion"), false)
        && price_is_zero(pricing.get("request"), true)
        && (!needs_images || price_is_zero(pricing.get("image"), true))
}

fn price_is_zero(value: Option<&Value>, missing_is_zero: bool) -> bool {
    match value {
        None | Some(Value::Null) => missing_is_zero,
        Some(Value::String(value)) => value.parse::<f64>().is_ok_and(|price| price == 0.0),
        Some(Value::Number(value)) => value.as_f64().is_some_and(|price| price == 0.0),
        _ => false,
    }
}

fn supports(model: &Value, pointer: &str, capability: &str) -> bool {
    model
        .pointer(pointer)
        .and_then(Value::as_array)
        .is_some_and(|items| items.iter().any(|item| item.as_str() == Some(capability)))
}

pub fn apply_free_model_fallback(payload: &mut Value, models: &[String]) {
    let Some(object) = payload.as_object_mut() else {
        return;
    };
    object.remove("model");
    object.insert("models".into(), serde_json::json!(models));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keeps_server_ranking_but_only_for_free_capable_models() {
        let payload = json!({
            "data": [
                {
                    "id": "best/paid",
                    "pricing": {"prompt": "0.1", "completion": "0", "request": "0"},
                    "supported_parameters": ["tools"],
                    "architecture": {"input_modalities": ["text", "image"]}
                },
                {
                    "id": "best/free-agent:free",
                    "pricing": {"prompt": "0", "completion": "0", "request": "0", "image": "0"},
                    "supported_parameters": ["tools"],
                    "architecture": {"input_modalities": ["text", "image"]}
                },
                {
                    "id": "text-only/free:free",
                    "pricing": {"prompt": "0", "completion": "0", "request": "0"},
                    "supported_parameters": ["tools"],
                    "architecture": {"input_modalities": ["text"]}
                }
            ]
        });

        assert_eq!(
            ranked_free_models_from_payload(&payload, true, true),
            vec!["best/free-agent:free", FREE_ROUTER]
        );
    }

    #[test]
    fn fallback_request_uses_ranked_models_instead_of_the_paid_model() {
        let mut payload = json!({"model": "paid/model", "messages": []});
        let models = vec!["best/free:free".into(), FREE_ROUTER.into()];
        apply_free_model_fallback(&mut payload, &models);
        assert!(payload.get("model").is_none());
        assert_eq!(payload["models"], json!(models));
        assert!(is_credit_exhausted(StatusCode::PAYMENT_REQUIRED));
        assert!(!is_credit_exhausted(StatusCode::TOO_MANY_REQUESTS));
    }
}
