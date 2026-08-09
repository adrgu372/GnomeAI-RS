//! Browser-mediated approvals for the shared WebTool/WhatsApp runtime.
//!
//! The model only receives the final allow/deny result. Sudo credentials use
//! `SecretString`, are never included in the public request, and are moved
//! directly into the privilege broker path.

use std::{collections::HashMap, fmt, sync::Arc, time::Duration};

use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast, oneshot};
use uuid::Uuid;

use crate::protocol::SecretString;

#[derive(Clone)]
pub struct PendingApprovals {
    inner: Arc<Mutex<HashMap<String, PendingApproval>>>,
    events: broadcast::Sender<Value>,
}

impl Default for PendingApprovals {
    fn default() -> Self {
        let (events, _) = broadcast::channel(64);
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            events,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingKind {
    Standard,
    Credential,
}

struct PendingApproval {
    scope: String,
    kind: PendingKind,
    public: Value,
    sender: Option<oneshot::Sender<ApprovalAnswer>>,
}

enum ApprovalAnswer {
    Allow,
    Deny,
    Credential {
        credential: Option<SecretString>,
        remember: bool,
    },
}

#[derive(Deserialize)]
pub struct ApprovalAnswerPayload {
    pub decision: String,
    #[serde(default)]
    pub credential: Option<SecretString>,
    #[serde(default)]
    pub remember: bool,
}

#[derive(Debug)]
pub enum PendingApprovalError {
    NotFound,
    Mismatch,
    AlreadyPending,
    Timeout,
    Closed,
    BadRequest(String),
}

impl PendingApprovalError {
    pub fn status_code(&self) -> axum::http::StatusCode {
        match self {
            Self::NotFound => axum::http::StatusCode::NOT_FOUND,
            Self::Mismatch | Self::AlreadyPending => axum::http::StatusCode::CONFLICT,
            Self::Timeout => axum::http::StatusCode::REQUEST_TIMEOUT,
            Self::Closed => axum::http::StatusCode::GONE,
            Self::BadRequest(_) => axum::http::StatusCode::BAD_REQUEST,
        }
    }
}

impl fmt::Display for PendingApprovalError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => write!(formatter, "No pending approval"),
            Self::Mismatch => write!(formatter, "Approval type mismatch"),
            Self::AlreadyPending => write!(formatter, "This conversation already needs approval"),
            Self::Timeout => write!(formatter, "Approval request timed out"),
            Self::Closed => write!(formatter, "Approval channel was closed"),
            Self::BadRequest(message) => write!(formatter, "{message}"),
        }
    }
}

impl std::error::Error for PendingApprovalError {}

impl PendingApprovals {
    /// Subscribe to newly registered public approval requests. The event never
    /// contains credentials; it is the same sanitized object WebTool polls.
    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.events.subscribe()
    }

    pub async fn public_view(&self) -> Value {
        let guard = self.inner.lock().await;
        let mut requests = guard
            .values()
            .map(|pending| pending.public.clone())
            .collect::<Vec<_>>();
        requests.sort_by(|left, right| {
            left.get("createdAt")
                .and_then(Value::as_str)
                .cmp(&right.get("createdAt").and_then(Value::as_str))
        });
        json!({
            "pending": !requests.is_empty(),
            "request": requests.first(),
            "requests": requests,
        })
    }

    /// Return requests owned by one conversation, including its subagents.
    /// A boundary check on `#` prevents similarly-prefixed chat ids from
    /// seeing or answering one another's approvals.
    pub async fn public_for_conversation(&self, conversation_scope: &str) -> Vec<Value> {
        if conversation_scope.trim().is_empty() {
            return Vec::new();
        }
        let guard = self.inner.lock().await;
        let mut requests = guard
            .values()
            .filter(|pending| {
                pending.scope == conversation_scope
                    || pending
                        .scope
                        .strip_prefix(conversation_scope)
                        .is_some_and(|suffix| suffix.starts_with('#'))
            })
            .map(|pending| pending.public.clone())
            .collect::<Vec<_>>();
        requests.sort_by(|left, right| {
            left.get("createdAt")
                .and_then(Value::as_str)
                .cmp(&right.get("createdAt").and_then(Value::as_str))
        });
        requests
    }

    pub async fn request_standard(
        &self,
        scope: &str,
        tool: &str,
        command: &str,
        reason: &str,
        workspace: &str,
    ) -> Result<bool, PendingApprovalError> {
        let public = json!({
            "kind": "approval",
            "tool": tool,
            "command": command,
            "reason": reason,
            "workspace": workspace,
        });
        match self
            .request(scope, PendingKind::Standard, public, 3_600)
            .await?
        {
            ApprovalAnswer::Allow => Ok(true),
            ApprovalAnswer::Deny => Ok(false),
            ApprovalAnswer::Credential { .. } => Err(PendingApprovalError::Mismatch),
        }
    }

    pub async fn request_credential(
        &self,
        scope: &str,
        command: &str,
        keyring_available: bool,
        attempt: u8,
        message: Option<String>,
    ) -> Result<Option<(SecretString, bool)>, PendingApprovalError> {
        let public = json!({
            "kind": "sudo_credential",
            "tool": "Sudo",
            "command": command,
            "keyringAvailable": keyring_available,
            "attempt": attempt,
            "message": message,
        });
        match self
            .request(scope, PendingKind::Credential, public, 3_600)
            .await?
        {
            ApprovalAnswer::Credential {
                credential,
                remember,
            } => Ok(credential.map(|secret| (secret, remember))),
            ApprovalAnswer::Deny => Ok(None),
            ApprovalAnswer::Allow => Err(PendingApprovalError::Mismatch),
        }
    }

    pub async fn answer(
        &self,
        id: &str,
        payload: ApprovalAnswerPayload,
    ) -> Result<Value, PendingApprovalError> {
        let mut guard = self.inner.lock().await;
        let Some(mut pending) = guard.remove(id) else {
            return Err(PendingApprovalError::NotFound);
        };
        let decision = payload.decision.trim().to_ascii_lowercase();
        let answer = match (pending.kind, decision.as_str()) {
            (PendingKind::Standard, "allow" | "approve") => ApprovalAnswer::Allow,
            (PendingKind::Standard, "deny" | "cancel") => ApprovalAnswer::Deny,
            (PendingKind::Credential, "submit" | "allow") => ApprovalAnswer::Credential {
                credential: payload.credential,
                remember: payload.remember,
            },
            (PendingKind::Credential, "deny" | "cancel") => ApprovalAnswer::Deny,
            _ => {
                guard.insert(id.to_string(), pending);
                return Err(PendingApprovalError::BadRequest(
                    "Invalid decision for this approval request".into(),
                ));
            }
        };
        let sender = pending.sender.take();
        drop(guard);
        if let Some(sender) = sender {
            let _ = sender.send(answer);
        }
        Ok(json!({"ok": true}))
    }

    async fn request(
        &self,
        scope: &str,
        kind: PendingKind,
        mut public: Value,
        timeout_seconds: u64,
    ) -> Result<ApprovalAnswer, PendingApprovalError> {
        let id = format!(
            "approval_{}",
            Uuid::new_v4()
                .simple()
                .to_string()
                .chars()
                .take(12)
                .collect::<String>()
        );
        let source = if scope.starts_with("wa_") {
            "whatsapp"
        } else {
            "webtool"
        };
        public["id"] = json!(id);
        public["scope"] = json!(scope);
        public["source"] = json!(source);
        public["createdAt"] = json!(Utc::now().to_rfc3339());
        let (sender, receiver) = oneshot::channel();
        let event = public.clone();
        {
            let mut guard = self.inner.lock().await;
            if guard.values().any(|pending| pending.scope == scope) {
                return Err(PendingApprovalError::AlreadyPending);
            }
            guard.insert(
                id.clone(),
                PendingApproval {
                    scope: scope.to_string(),
                    kind,
                    public,
                    sender: Some(sender),
                },
            );
        }
        // WebTool continues to poll `public_view`; subscribers are used by
        // conversational channels such as WhatsApp to surface the request as
        // soon as the tool loop blocks.
        let _ = self.events.send(event);

        match tokio::time::timeout(Duration::from_secs(timeout_seconds), receiver).await {
            Ok(Ok(answer)) => Ok(answer),
            Ok(Err(_)) => Err(PendingApprovalError::Closed),
            Err(_) => {
                self.inner.lock().await.remove(&id);
                Err(PendingApprovalError::Timeout)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn standard_approval_exposes_no_private_answer() {
        let pending = PendingApprovals::default();
        let worker = pending.clone();
        let request = tokio::spawn(async move {
            worker
                .request_standard("wa_chat", "Bash", "cargo test", "runs tests", "/project")
                .await
        });
        tokio::task::yield_now().await;
        let public = pending.public_view().await;
        assert_eq!(public["request"]["source"], "whatsapp");
        let id = public["request"]["id"].as_str().unwrap();
        pending
            .answer(
                id,
                ApprovalAnswerPayload {
                    decision: "allow".into(),
                    credential: None,
                    remember: false,
                },
            )
            .await
            .unwrap();
        assert!(request.await.unwrap().unwrap());
    }

    #[tokio::test]
    async fn credential_moves_as_a_secret_without_entering_public_json() {
        let pending = PendingApprovals::default();
        let worker = pending.clone();
        let request = tokio::spawn(async move {
            worker
                .request_credential("web_chat", "apt update", true, 1, None)
                .await
        });
        tokio::task::yield_now().await;
        let public = pending.public_view().await;
        assert!(!public.to_string().contains("secret-value"));
        let id = public["request"]["id"].as_str().unwrap();
        pending
            .answer(
                id,
                ApprovalAnswerPayload {
                    decision: "submit".into(),
                    credential: Some(SecretString::new("secret-value")),
                    remember: true,
                },
            )
            .await
            .unwrap();
        let (secret, remember) = request.await.unwrap().unwrap().unwrap();
        assert_eq!(secret.expose(), "secret-value");
        assert!(remember);
    }

    #[tokio::test]
    async fn conversation_view_includes_subagents_but_not_similar_chat_ids() {
        let pending = PendingApprovals::default();
        let worker = pending.clone();
        let request = tokio::spawn(async move {
            worker
                .request_standard("wa_chat#a1", "Bash", "cargo test", "runs tests", "/project")
                .await
        });
        tokio::task::yield_now().await;

        let requests = pending.public_for_conversation("wa_chat").await;
        assert_eq!(requests.len(), 1);
        assert!(pending.public_for_conversation("wa_cha").await.is_empty());

        pending
            .answer(
                requests[0]["id"].as_str().unwrap(),
                ApprovalAnswerPayload {
                    decision: "deny".into(),
                    credential: None,
                    remember: false,
                },
            )
            .await
            .unwrap();
        assert!(!request.await.unwrap().unwrap());
    }

    #[tokio::test]
    async fn new_requests_are_broadcast_without_private_answers() {
        let pending = PendingApprovals::default();
        let mut events = pending.subscribe();
        let worker = pending.clone();
        let request = tokio::spawn(async move {
            worker
                .request_standard("wa_chat", "Edit", "src/main.rs", "edits a file", "/project")
                .await
        });

        let event = events.recv().await.unwrap();
        assert_eq!(event["source"], "whatsapp");
        assert_eq!(event["kind"], "approval");
        assert!(!event.to_string().contains("credential"));
        pending
            .answer(
                event["id"].as_str().unwrap(),
                ApprovalAnswerPayload {
                    decision: "allow".into(),
                    credential: None,
                    remember: false,
                },
            )
            .await
            .unwrap();
        assert!(request.await.unwrap().unwrap());
    }
}
