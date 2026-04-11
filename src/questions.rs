use std::{collections::HashMap, fmt, sync::Arc, time::Duration};

use chrono::Utc;
use serde_json::{Map, Value, json};
use tokio::sync::{Mutex, oneshot};
use uuid::Uuid;

#[derive(Clone, Default)]
pub struct PendingQuestions {
    inner: Arc<Mutex<HashMap<String, PendingQuestion>>>,
}

struct PendingQuestion {
    public: Value,
    questions: Vec<Value>,
    sender: Option<oneshot::Sender<Value>>,
}

#[derive(Debug)]
pub enum PendingQuestionError {
    NotFound,
    Mismatch,
    BadRequest(String),
    AlreadyPending,
    Timeout,
    Closed,
}

impl PendingQuestionError {
    pub fn status_code(&self) -> axum::http::StatusCode {
        match self {
            Self::NotFound => axum::http::StatusCode::NOT_FOUND,
            Self::Mismatch => axum::http::StatusCode::CONFLICT,
            Self::BadRequest(_) => axum::http::StatusCode::BAD_REQUEST,
            Self::AlreadyPending => axum::http::StatusCode::CONFLICT,
            Self::Timeout => axum::http::StatusCode::REQUEST_TIMEOUT,
            Self::Closed => axum::http::StatusCode::GONE,
        }
    }
}

impl fmt::Display for PendingQuestionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => write!(f, "No pending question"),
            Self::Mismatch => write!(f, "Question mismatch"),
            Self::BadRequest(message) => write!(f, "{message}"),
            Self::AlreadyPending => write!(f, "There is already a pending user question"),
            Self::Timeout => write!(f, "Timed out waiting for user answer"),
            Self::Closed => write!(f, "Question answer channel was closed"),
        }
    }
}

impl std::error::Error for PendingQuestionError {}

impl PendingQuestions {
    pub async fn public_view(&self, scope: &str) -> Value {
        let guard = self.inner.lock().await;
        match guard.get(scope) {
            Some(state) => json!({"pending": true, "question": state.public}),
            None => json!({"pending": false}),
        }
    }

    pub async fn ask(
        &self,
        scope: &str,
        raw_questions: &Value,
        timeout_seconds: u64,
    ) -> Result<Value, PendingQuestionError> {
        let questions = sanitize_question_payload(raw_questions)?;
        let id = format!(
            "q_{}",
            Uuid::new_v4()
                .simple()
                .to_string()
                .chars()
                .take(10)
                .collect::<String>()
        );
        let public = json!({
            "id": id,
            "scope": scope,
            "createdAt": workflow_timestamp(),
            "questions": questions,
        });
        let (sender, receiver) = oneshot::channel();
        {
            let mut guard = self.inner.lock().await;
            if guard.contains_key(scope) {
                return Err(PendingQuestionError::AlreadyPending);
            }
            guard.insert(
                scope.to_string(),
                PendingQuestion {
                    public,
                    questions,
                    sender: Some(sender),
                },
            );
        }

        let bounded_timeout = timeout_seconds.clamp(30, 86_400);
        match tokio::time::timeout(Duration::from_secs(bounded_timeout), receiver).await {
            Ok(Ok(result)) => Ok(result),
            Ok(Err(_)) => Err(PendingQuestionError::Closed),
            Err(_) => {
                let mut guard = self.inner.lock().await;
                if guard
                    .get(scope)
                    .and_then(|state| state.public.get("id"))
                    .and_then(Value::as_str)
                    == Some(id.as_str())
                {
                    guard.remove(scope);
                }
                Err(PendingQuestionError::Timeout)
            }
        }
    }

    pub async fn answer(
        &self,
        scope: &str,
        payload: &Value,
    ) -> Result<Value, PendingQuestionError> {
        let obj = payload
            .as_object()
            .ok_or_else(|| PendingQuestionError::BadRequest("payload must be an object".into()))?;
        let question_id = obj.get("id").and_then(Value::as_str).map(normalize_ws);
        let raw_answers = obj
            .get("answers")
            .and_then(Value::as_object)
            .ok_or_else(|| PendingQuestionError::BadRequest("answers must be an object".into()))?;
        let raw_annotations = obj
            .get("annotations")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();

        let mut guard = self.inner.lock().await;
        let Some(state) = guard.get_mut(scope) else {
            return Err(PendingQuestionError::NotFound);
        };
        let current_id = state.public.get("id").and_then(Value::as_str).unwrap_or("");
        if question_id
            .as_ref()
            .is_some_and(|incoming| incoming != current_id)
        {
            return Err(PendingQuestionError::Mismatch);
        }

        let result = normalize_answer_payload(&state.questions, raw_answers, &raw_annotations)?;
        let sender = state.sender.take();
        guard.remove(scope);
        drop(guard);
        if let Some(sender) = sender {
            let _ = sender.send(result.clone());
        }
        Ok(json!({"ok": true}))
    }
}

fn sanitize_question_payload(raw_questions: &Value) -> Result<Vec<Value>, PendingQuestionError> {
    let questions = raw_questions.as_array().ok_or_else(|| {
        PendingQuestionError::BadRequest("questions must be a non-empty list".into())
    })?;
    if questions.is_empty() {
        return Err(PendingQuestionError::BadRequest(
            "questions must be a non-empty list".into(),
        ));
    }
    if questions.len() > 4 {
        return Err(PendingQuestionError::BadRequest(
            "At most 4 questions are allowed".into(),
        ));
    }

    let mut normalized = Vec::new();
    let mut seen_headers = Vec::new();
    for (idx, item) in questions.iter().enumerate() {
        let obj = item.as_object().ok_or_else(|| {
            PendingQuestionError::BadRequest(format!("questions[{}] must be an object", idx + 1))
        })?;
        let question = text_field(obj, "question");
        let header = text_field(obj, "header")
            .chars()
            .take(24)
            .collect::<String>();
        let multi_select = obj
            .get("multiSelect")
            .or_else(|| obj.get("multi_select"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if question.is_empty() {
            return Err(PendingQuestionError::BadRequest(format!(
                "questions[{}].question is required",
                idx + 1
            )));
        }
        if header.is_empty() {
            return Err(PendingQuestionError::BadRequest(format!(
                "questions[{}].header is required",
                idx + 1
            )));
        }
        if seen_headers.contains(&header) {
            return Err(PendingQuestionError::BadRequest(
                "Question headers must be unique".into(),
            ));
        }
        seen_headers.push(header.clone());

        let options = obj
            .get("options")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                PendingQuestionError::BadRequest(format!(
                    "questions[{}].options must contain 2-4 options",
                    idx + 1
                ))
            })?;
        if !(2..=4).contains(&options.len()) {
            return Err(PendingQuestionError::BadRequest(format!(
                "questions[{}].options must contain 2-4 options",
                idx + 1
            )));
        }

        let mut cleaned_options = Vec::new();
        let mut seen_labels = Vec::new();
        for (opt_idx, option) in options.iter().enumerate() {
            let opt_obj = option.as_object().ok_or_else(|| {
                PendingQuestionError::BadRequest(format!(
                    "questions[{}].options[{}] must be an object",
                    idx + 1,
                    opt_idx + 1
                ))
            })?;
            let label = text_field(opt_obj, "label");
            let description = text_field(opt_obj, "description");
            let preview = opt_obj
                .get("preview")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            if label.is_empty() {
                return Err(PendingQuestionError::BadRequest(format!(
                    "questions[{}].options[{}].label is required",
                    idx + 1,
                    opt_idx + 1
                )));
            }
            if seen_labels.contains(&label) {
                return Err(PendingQuestionError::BadRequest(format!(
                    "questions[{}] option labels must be unique",
                    idx + 1
                )));
            }
            seen_labels.push(label.clone());
            cleaned_options.push(json!({
                "label": label,
                "description": description,
                "preview": preview,
            }));
        }
        normalized.push(json!({
            "question": question,
            "header": header,
            "options": cleaned_options,
            "multiSelect": multi_select,
        }));
    }
    Ok(normalized)
}

fn normalize_answer_payload(
    questions: &[Value],
    raw_answers: &Map<String, Value>,
    raw_annotations: &Map<String, Value>,
) -> Result<Value, PendingQuestionError> {
    let mut answers = Map::new();
    let mut annotations = Map::new();
    for question in questions {
        let key = question
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or("");
        let multi_select = question
            .get("multiSelect")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let answer_value = raw_answers.get(key);
        let answer_text = if multi_select {
            match answer_value {
                Some(Value::Array(items)) => items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(normalize_ws)
                    .filter(|item| !item.is_empty())
                    .collect::<Vec<_>>()
                    .join(", "),
                Some(value) => value.as_str().map(normalize_ws).unwrap_or_default(),
                None => String::new(),
            }
        } else {
            answer_value
                .and_then(Value::as_str)
                .map(normalize_ws)
                .unwrap_or_default()
        };
        if answer_text.is_empty() {
            return Err(PendingQuestionError::BadRequest(format!(
                "Missing answer for question: {key}"
            )));
        }
        answers.insert(key.to_string(), json!(answer_text));

        if let Some(annotation) = raw_annotations.get(key).and_then(Value::as_object) {
            let notes = annotation
                .get("notes")
                .and_then(Value::as_str)
                .map(normalize_ws)
                .unwrap_or_default();
            let preview = annotation
                .get("preview")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim()
                .to_string();
            if !notes.is_empty() || !preview.is_empty() {
                annotations.insert(key.to_string(), json!({"notes": notes, "preview": preview}));
            }
        }
    }

    Ok(json!({
        "questions": questions,
        "answers": answers,
        "annotations": annotations,
    }))
}

fn text_field(obj: &Map<String, Value>, key: &str) -> String {
    obj.get(key)
        .and_then(Value::as_str)
        .map(normalize_ws)
        .unwrap_or_default()
}

fn normalize_ws(text: &str) -> String {
    regex::Regex::new(r"\s+")
        .unwrap()
        .replace_all(text, " ")
        .trim()
        .to_string()
}

fn workflow_timestamp() -> String {
    Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}
