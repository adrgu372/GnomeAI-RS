use std::{
    collections::{HashMap, HashSet},
    fs,
};

use anyhow::Context;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::RwLock;
use tracing::warn;
use uuid::Uuid;

use crate::{
    config::AppConfig,
    llama::LlamaClient,
    storage::{AppPaths, Chat, ChatMessage, write_private},
};

const MEMORY_EXTRACTOR_PROMPT: &str = r#"You are analyzing a conversation. Extract persistent, important facts about the user that should be remembered across future conversations. Follow the rules:
- Extract only facts likely to be useful later (preferences, constraints, project info, goals, decisions).
- Do NOT extract one-time or obvious statements.
- For each fact, assign confidence (0.0-1.0). Higher confidence for explicit statements.
- If a fact contradicts an existing memory, output the newer one with higher confidence.
Output JSON:
{
  "new_facts": [{"fact": "...", "confidence": 0.95, "category": "UserPreference"}],
  "updated_facts": [{"id": "...", "fact": "...", "confidence": 0.98, "category": "UserPreference"}],
  "facts_to_forget": ["id1", "id2"]
}
Return JSON only."#;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum MemoryCategory {
    UserPreference,
    ProjectInfo,
    PersonalInfo,
    Goal,
    Decision,
}

impl MemoryCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::UserPreference => "UserPreference",
            Self::ProjectInfo => "ProjectInfo",
            Self::PersonalInfo => "PersonalInfo",
            Self::Goal => "Goal",
            Self::Decision => "Decision",
        }
    }

    fn priority(&self) -> i64 {
        match self {
            Self::UserPreference => 30,
            Self::Goal => 26,
            Self::ProjectInfo => 24,
            Self::PersonalInfo => 22,
            Self::Decision => 20,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryFact {
    pub id: String,
    pub fact: String,
    pub confidence: f32,
    pub source_chat_id: String,
    pub timestamp: DateTime<Utc>,
    pub last_accessed: DateTime<Utc>,
    pub access_count: u32,
    pub category: MemoryCategory,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentConversationSummary {
    pub chat_id: String,
    pub title: String,
    pub snippet: String,
    pub updated_at: DateTime<Utc>,
    pub keywords: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MemoryStore {
    pub facts: Vec<MemoryFact>,
    pub recent_conversations: Vec<RecentConversationSummary>,
}

#[derive(Debug, Clone)]
pub struct WorkingMemorySnapshot {
    pub session_notes: Vec<String>,
    pub facts: Vec<MemoryFact>,
    pub recent_conversations: Vec<RecentConversationSummary>,
}

impl WorkingMemorySnapshot {
    pub fn is_empty(&self) -> bool {
        self.session_notes.is_empty()
            && self.facts.is_empty()
            && self.recent_conversations.is_empty()
    }

    pub fn prompt_block(&self) -> String {
        if self.is_empty() {
            return String::new();
        }
        let mut lines = vec![
            "Cross-Conversation Memory".to_string(),
            "This block is persistent memory across chats. Use it as helpful context, but prefer the current user message if there is any conflict.".to_string(),
        ];
        if !self.session_notes.is_empty() {
            lines.push("Session Notes:".into());
            for note in &self.session_notes {
                lines.push(format!("- {note}"));
            }
        }
        if !self.facts.is_empty() {
            lines.push("Remembered Facts:".into());
            for fact in &self.facts {
                lines.push(format!(
                    "- [{} | conf {:.2}] {}",
                    fact.category.as_str(),
                    fact.confidence,
                    fact.fact
                ));
            }
        }
        if !self.recent_conversations.is_empty() {
            lines.push("Related Recent Conversations:".into());
            for item in &self.recent_conversations {
                lines.push(format!(
                    "- [{}] {}: {}",
                    item.chat_id, item.title, item.snippet
                ));
            }
        }
        lines.join("\n")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ExtractedFactCandidate {
    fact: String,
    confidence: f32,
    category: MemoryCategory,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UpdatedFactCandidate {
    id: String,
    fact: String,
    confidence: f32,
    category: Option<MemoryCategory>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct MemoryExtractionResult {
    #[serde(default)]
    new_facts: Vec<ExtractedFactCandidate>,
    #[serde(default)]
    updated_facts: Vec<UpdatedFactCandidate>,
    #[serde(default)]
    facts_to_forget: Vec<String>,
}

impl MemoryStore {
    pub fn load(paths: &AppPaths) -> anyhow::Result<Self> {
        if !paths.memory_store_file.exists() {
            return Ok(Self::default());
        }
        let raw = fs::read_to_string(&paths.memory_store_file)
            .with_context(|| format!("failed to read {}", paths.memory_store_file.display()))?;
        let mut store: Self = serde_json::from_str(&raw)
            .with_context(|| format!("failed to parse {}", paths.memory_store_file.display()))?;
        store.normalize();
        Ok(store)
    }

    pub fn save(&self, paths: &AppPaths) -> anyhow::Result<()> {
        let raw = serde_json::to_string_pretty(self)?;
        write_private(&paths.memory_store_file, raw.as_bytes())
            .with_context(|| format!("failed to write {}", paths.memory_store_file.display()))
    }

    /// Wipe everything and persist the empty store.
    pub fn clear(&mut self, paths: &AppPaths) -> anyhow::Result<()> {
        self.facts.clear();
        self.recent_conversations.clear();
        self.save(paths)
    }

    pub fn normalize(&mut self) {
        let mut cleaned_facts = Vec::new();
        for mut fact in self.facts.drain(..) {
            fact.confidence = fact.confidence.clamp(0.0, 1.0);
            if let Some((normalized_fact, normalized_category)) =
                normalize_fact_for_storage(&fact.fact, &fact.category)
            {
                fact.fact = normalized_fact;
                fact.category = normalized_category;
                cleaned_facts.push(fact);
            }
        }
        self.facts = cleaned_facts;
        dedupe_facts(&mut self.facts);
        self.recent_conversations.retain(|item| {
            !item.chat_id.trim().is_empty()
                && !item.snippet.trim().is_empty()
                && !looks_like_secret(&item.title)
                && !looks_like_secret(&item.snippet)
        });
        self.recent_conversations.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| a.chat_id.cmp(&b.chat_id))
        });
        self.recent_conversations
            .dedup_by(|a, b| a.chat_id == b.chat_id);
    }

    pub fn update_recent_conversation(&mut self, chat: &Chat, cfg: &AppConfig) {
        let summary = summarize_chat(chat);
        if summary.snippet.is_empty() {
            return;
        }
        self.recent_conversations
            .retain(|item| item.chat_id != chat.id);
        self.recent_conversations.push(summary);
        self.recent_conversations.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| a.chat_id.cmp(&b.chat_id))
        });
        self.recent_conversations
            .truncate(cfg.memory_max_recent_summaries_stored);
    }

    pub fn build_working_memory(
        &mut self,
        cfg: &AppConfig,
        query: &str,
        history: &[ChatMessage],
        chat_id: Option<&str>,
    ) -> WorkingMemorySnapshot {
        let mut session_notes = Vec::new();
        if let Some(chat_id) = chat_id.filter(|item| !item.trim().is_empty()) {
            session_notes.push(format!("Current chat id: {chat_id}"));
        }
        session_notes
            .push("Persistent memory is stored locally and shared across conversations.".into());
        if cfg.memory_max_age_days == 0 {
            session_notes.push("Memory age filter: no age limit.".into());
        } else {
            session_notes.push(format!(
                "Memory age filter: only items from the last {} days.",
                cfg.memory_max_age_days
            ));
        }
        session_notes.push(
            "Prefer newer/current user instructions over older memory if they conflict.".into(),
        );

        let query_terms = combined_terms(query, history);
        let now = Utc::now();
        let cutoff = memory_cutoff(cfg, now);
        let mut scored_facts = self
            .facts
            .iter()
            .enumerate()
            .filter(|(_, fact)| cutoff.is_none_or(|cutoff| fact.timestamp >= cutoff))
            .map(|(index, fact)| (score_fact(fact, &query_terms, chat_id), index))
            .collect::<Vec<_>>();
        scored_facts.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let mut selected_fact_indexes = scored_facts
            .into_iter()
            .filter(|(score, _)| *score > 0)
            .take(cfg.memory_max_facts_in_prompt)
            .map(|(_, index)| index)
            .collect::<Vec<_>>();
        selected_fact_indexes.sort_unstable();
        let mut selected_facts = Vec::new();
        for index in selected_fact_indexes {
            if let Some(fact) = self.facts.get_mut(index) {
                fact.access_count = fact.access_count.saturating_add(1);
                fact.last_accessed = now;
                selected_facts.push(fact.clone());
            }
        }

        let mut scored_recent = self
            .recent_conversations
            .iter()
            .filter(|item| Some(item.chat_id.as_str()) != chat_id)
            .filter(|item| cutoff.is_none_or(|cutoff| item.updated_at >= cutoff))
            .map(|item| (score_recent_summary(item, &query_terms), item.clone()))
            .collect::<Vec<_>>();
        scored_recent.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| b.1.updated_at.cmp(&a.1.updated_at))
        });
        let recent_conversations = scored_recent
            .into_iter()
            .filter(|(score, _)| *score > 0)
            .take(cfg.memory_max_recent_summaries_in_prompt)
            .map(|(_, item)| item)
            .collect::<Vec<_>>();

        WorkingMemorySnapshot {
            session_notes,
            facts: selected_facts,
            recent_conversations,
        }
    }

    fn apply_extraction_result(
        &mut self,
        cfg: &AppConfig,
        chat_id: &str,
        extraction: MemoryExtractionResult,
    ) {
        let now = Utc::now();
        let forget = extraction
            .facts_to_forget
            .into_iter()
            .collect::<HashSet<_>>();
        if !forget.is_empty() {
            self.facts.retain(|fact| !forget.contains(&fact.id));
        }

        for item in extraction.updated_facts {
            let Some((new_fact, normalized_category)) = normalize_fact_for_storage(
                &item.fact,
                item.category.as_ref().unwrap_or(&MemoryCategory::Decision),
            ) else {
                continue;
            };
            if let Some(fact) = self.facts.iter_mut().find(|fact| fact.id == item.id) {
                fact.fact = new_fact;
                fact.confidence = item.confidence.clamp(0.0, 1.0);
                fact.timestamp = now;
                fact.last_accessed = now;
                fact.source_chat_id = chat_id.to_string();
                fact.category = item.category.unwrap_or(normalized_category);
            }
        }

        for item in extraction.new_facts {
            let Some((fact_text, category)) =
                normalize_fact_for_storage(&item.fact, &item.category)
            else {
                continue;
            };
            let confidence = item.confidence.clamp(0.0, 1.0);
            if let Some(index) = find_similar_fact(&self.facts, &fact_text, Some(&category)) {
                if let Some(existing) = self.facts.get_mut(index) {
                    if confidence >= existing.confidence || existing.fact != fact_text {
                        existing.fact = fact_text;
                        existing.confidence = confidence;
                        existing.category = category.clone();
                        existing.timestamp = now;
                        existing.last_accessed = now;
                        existing.source_chat_id = chat_id.to_string();
                    }
                }
                continue;
            }
            self.facts.push(MemoryFact {
                id: format!("mem_{}", Uuid::new_v4().simple()),
                fact: fact_text,
                confidence,
                source_chat_id: chat_id.to_string(),
                timestamp: now,
                last_accessed: now,
                access_count: 0,
                category,
            });
        }

        self.normalize();
        self.facts.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.last_accessed.cmp(&a.last_accessed))
                .then_with(|| b.access_count.cmp(&a.access_count))
        });
        self.facts.truncate(cfg.memory_max_facts_stored);
    }
}

pub async fn build_working_memory_block(
    store_state: &std::sync::Arc<RwLock<MemoryStore>>,
    paths: &AppPaths,
    cfg: &AppConfig,
    query: &str,
    history: &[ChatMessage],
    chat_id: Option<&str>,
) -> anyhow::Result<String> {
    if !cfg.memory_enabled {
        return Ok(String::new());
    }
    let mut store = store_state.write().await;
    let snapshot = store.build_working_memory(cfg, query, history, chat_id);
    store.save(paths)?;
    Ok(snapshot.prompt_block())
}

/// Compact memory block for the coding agent's system prompt: the top facts
/// by confidence inside the shared age filter. No query scoring — a coding
/// session has no "current question" yet when the prompt is built.
pub fn agent_memory_block(store: &MemoryStore, cfg: &AppConfig) -> String {
    if !cfg.memory_enabled {
        return String::new();
    }
    let cutoff = memory_cutoff(cfg, Utc::now());
    let mut facts: Vec<&MemoryFact> = store
        .facts
        .iter()
        .filter(|fact| cutoff.is_none_or(|cutoff| fact.timestamp >= cutoff))
        .collect();
    facts.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.timestamp.cmp(&a.timestamp))
    });
    facts.truncate(cfg.memory_max_facts_in_prompt);
    if facts.is_empty() {
        return String::new();
    }
    let mut lines = vec![
        "Cross-Conversation Memory (shared with WebTool)".to_string(),
        "Persistent facts about the user. Prefer the current conversation when they conflict."
            .to_string(),
    ];
    for fact in facts {
        lines.push(format!("- [{}] {}", fact.category.as_str(), fact.fact));
    }
    lines.join("\n")
}

pub fn append_memory_block(base_prompt: &str, memory_block: Option<&str>) -> String {
    let memory_block = memory_block
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .unwrap_or("");
    if memory_block.is_empty() {
        base_prompt.trim().to_string()
    } else {
        format!("{}\n\n{}", base_prompt.trim(), memory_block)
    }
}

pub async fn update_recent_memory_snapshot(
    store_state: &std::sync::Arc<RwLock<MemoryStore>>,
    paths: &AppPaths,
    cfg: &AppConfig,
    chat: &Chat,
) -> anyhow::Result<()> {
    if !cfg.memory_enabled {
        return Ok(());
    }
    let mut store = store_state.write().await;
    store.update_recent_conversation(chat, cfg);
    store.save(paths)
}

pub async fn refresh_memory_from_chat(
    client: &LlamaClient,
    cfg: &AppConfig,
    paths: &AppPaths,
    store_state: &std::sync::Arc<RwLock<MemoryStore>>,
    model: &str,
    chat: &Chat,
) -> anyhow::Result<()> {
    if !cfg.memory_enabled {
        return Ok(());
    }

    let conversation = render_conversation_for_extraction(chat, cfg.memory_extract_message_window);
    if conversation.trim().is_empty() {
        return Ok(());
    }

    let existing_facts = {
        let store = store_state.read().await;
        let cutoff = memory_cutoff(cfg, Utc::now());
        store
            .facts
            .iter()
            .filter(|fact| cutoff.is_none_or(|cutoff| fact.timestamp >= cutoff))
            .take(cfg.memory_max_existing_facts_for_extraction)
            .map(|fact| {
                json!({
                    "id": fact.id,
                    "fact": fact.fact,
                    "confidence": fact.confidence,
                    "category": fact.category.as_str(),
                })
            })
            .collect::<Vec<_>>()
    };
    let prompt = format!(
        "{MEMORY_EXTRACTOR_PROMPT}\nConversation: {conversation}\nExisting memories: {}",
        serde_json::to_string_pretty(&existing_facts).unwrap_or_else(|_| "[]".into())
    );
    let response = client
        .chat(
            cfg,
            model,
            vec![
                json!({"role": "system", "content": "Extract persistent user memory. Return valid JSON only."}),
                json!({"role": "user", "content": prompt}),
            ],
            0.1,
        )
        .await
        .context("memory extraction model error")?;
    let extraction = match parse_extraction_response(&response.content) {
        Ok(extraction) => extraction,
        Err(err) => {
            warn!("Memory extraction parse failed: {err}");
            return Ok(());
        }
    };

    let mut store = store_state.write().await;
    store.update_recent_conversation(chat, cfg);
    store.apply_extraction_result(cfg, &chat.id, extraction);
    store.save(paths)
}

fn memory_cutoff(cfg: &AppConfig, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    (cfg.memory_max_age_days > 0)
        .then(|| now - ChronoDuration::days(i64::from(cfg.memory_max_age_days)))
}

fn render_conversation_for_extraction(chat: &Chat, window: usize) -> String {
    let start = chat.messages.len().saturating_sub(window);
    let mut lines = vec![format!("Chat title: {}", chat.title)];
    for message in &chat.messages[start..] {
        if message.role != "user" && !assistant_message_is_memory_confirmation(message) {
            continue;
        }
        let role = if message.role == "user" {
            "User"
        } else {
            "Assistant"
        };
        let text = message_to_text(message);
        if text.is_empty() {
            continue;
        }
        lines.push(format!("{role}: {}", preview(&text, 500)));
    }
    lines.join("\n")
}

fn assistant_message_is_memory_confirmation(message: &ChatMessage) -> bool {
    let Value::String(text) = &message.content else {
        return false;
    };
    let lowered = normalize_for_match(text);
    lowered.contains("am retinut")
        || lowered.contains("am reținut")
        || lowered.contains("noted")
        || lowered.contains("voi tine minte")
        || lowered.contains("i will remember")
}

fn message_to_text(message: &ChatMessage) -> String {
    match &message.content {
        Value::String(text) => {
            if text.starts_with("[Extracted content from uploaded file:") {
                String::new()
            } else {
                normalize_ws(text)
            }
        }
        Value::Object(obj) => {
            let file_type = obj.get("type").and_then(Value::as_str).unwrap_or("file");
            let filename = obj
                .get("filename")
                .and_then(Value::as_str)
                .unwrap_or("file");
            format!("[uploaded {file_type}: {filename}]")
        }
        other => preview(&other.to_string(), 200),
    }
}

fn summarize_chat(chat: &Chat) -> RecentConversationSummary {
    let mut parts = Vec::new();
    for message in chat.messages.iter().rev() {
        let text = message_to_text(message);
        if text.is_empty() || looks_like_secret(&text) {
            continue;
        }
        parts.push(text);
        if parts.len() >= 3 {
            break;
        }
    }
    parts.reverse();
    let snippet = preview(&parts.join(" | "), 260);
    let title = if looks_like_secret(&chat.title) {
        "Conversation".to_string()
    } else {
        normalize_ws(&chat.title)
    };
    let keywords = tokenize_terms(&format!("{title} {snippet}"))
        .into_iter()
        .take(12)
        .collect::<Vec<_>>();
    RecentConversationSummary {
        chat_id: chat.id.clone(),
        title,
        snippet,
        updated_at: chat
            .messages
            .last()
            .map(|message| message.timestamp)
            .unwrap_or(chat.created),
        keywords,
    }
}

fn score_fact(fact: &MemoryFact, query_terms: &HashSet<String>, chat_id: Option<&str>) -> i64 {
    let fact_terms = tokenize_terms(&fact.fact)
        .into_iter()
        .collect::<HashSet<_>>();
    let overlap = query_terms.intersection(&fact_terms).count() as i64;
    let recency_days = (Utc::now() - fact.last_accessed).num_days().max(0);
    let recency_score = 14_i64.saturating_sub(recency_days.min(14));
    let same_chat_bonus = chat_id.is_some_and(|item| item == fact.source_chat_id) as i64 * 8;
    (fact.confidence * 100.0) as i64
        + fact.category.priority()
        + recency_score
        + (fact.access_count as i64 * 2)
        + (overlap * 15)
        + same_chat_bonus
}

fn score_recent_summary(item: &RecentConversationSummary, query_terms: &HashSet<String>) -> i64 {
    let item_terms = item.keywords.iter().cloned().collect::<HashSet<_>>();
    let overlap = query_terms.intersection(&item_terms).count() as i64;
    let recency_days = (Utc::now() - item.updated_at).num_days().max(0);
    let recency_score = 10_i64.saturating_sub(recency_days.min(10));
    overlap * 18 + recency_score + 1
}

fn parse_extraction_response(text: &str) -> anyhow::Result<MemoryExtractionResult> {
    let json_text =
        extract_json_object(text).ok_or_else(|| anyhow::anyhow!("no JSON object found"))?;
    let mut parsed: MemoryExtractionResult =
        serde_json::from_str(&json_text).context("failed to parse extraction JSON")?;
    parsed
        .new_facts
        .retain(|item| !normalize_ws(&item.fact).is_empty());
    parsed
        .updated_facts
        .retain(|item| !item.id.trim().is_empty() && !normalize_ws(&item.fact).is_empty());
    Ok(parsed)
}

/// True when a candidate fact looks like a credential. Secrets never enter
/// cross-conversation memory, regardless of extractor confidence — the store
/// is plain JSON on disk and gets pasted into prompts.
pub fn looks_like_secret(text: &str) -> bool {
    let lowered = text.to_lowercase();
    const MARKERS: &[&str] = &[
        "api key",
        "api_key",
        "apikey",
        "cheie api",
        "cheia api",
        "secret",
        "password",
        "parola",
        "passphrase",
        "private key",
        "access token",
        "auth token",
        "api token",
        "refresh token",
        "bearer ",
        "credential",
    ];
    if MARKERS.iter().any(|marker| lowered.contains(marker)) {
        return true;
    }

    for word in text.split_whitespace() {
        let trimmed =
            word.trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '-' && ch != '_');
        if trimmed.len() < 12 {
            continue;
        }
        let lowered_word = trimmed.to_lowercase();
        const PREFIXES: &[&str] = &[
            "sk-",
            "sk_",
            "pk_",
            "ghp_",
            "gho_",
            "github_pat_",
            "xoxb-",
            "xoxp-",
            "akia",
            "ya29.",
            "eyj",
        ];
        if PREFIXES
            .iter()
            .any(|prefix| lowered_word.starts_with(prefix))
        {
            return true;
        }
        // Long mixed-alphanumeric blobs with no spaces are almost always keys.
        if trimmed.len() >= 32
            && trimmed
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
            && trimmed.chars().any(|ch| ch.is_ascii_digit())
            && trimmed.chars().any(|ch| ch.is_ascii_uppercase())
            && trimmed.chars().any(|ch| ch.is_ascii_lowercase())
        {
            return true;
        }
    }
    false
}

fn normalize_fact_for_storage(
    fact: &str,
    category: &MemoryCategory,
) -> Option<(String, MemoryCategory)> {
    let normalized_fact = normalize_ws(fact)
        .trim_matches(|ch: char| ch.is_whitespace() || ch == '"' || ch == '\'' || ch == '.')
        .to_string();
    if normalized_fact.len() < 12 || normalized_fact.len() > 220 {
        return None;
    }
    if looks_like_secret(&normalized_fact) {
        return None;
    }
    let normalized_category = infer_memory_category(&normalized_fact, category);
    if !is_persistent_memory_fact(&normalized_fact, &normalized_category) {
        return None;
    }
    Some((normalized_fact, normalized_category))
}

fn infer_memory_category(fact: &str, fallback: &MemoryCategory) -> MemoryCategory {
    let lowered = normalize_for_match(fact);
    if contains_any(
        &lowered,
        &[
            "intel",
            "amd",
            "nvidia",
            "rtx",
            "gtx",
            "cpu",
            "gpu",
            "ram",
            "vram",
            "linux",
            "windows",
            "ubuntu",
            "pcie",
            "ssd",
            "hdd",
            "hardware",
            "procesor",
            "placa video",
            "placa grafica",
        ],
    ) {
        return MemoryCategory::PersonalInfo;
    }
    if contains_any(
        &lowered,
        &[
            "prefer",
            "prefers",
            "romana",
            "romanian",
            "limba",
            "language",
            "stil",
            "style",
            "raspuns",
            "response style",
        ],
    ) {
        return MemoryCategory::UserPreference;
    }
    if contains_any(
        &lowered,
        &[
            "does not use",
            "nu foloseste",
            "nu foloseste",
            "nu utilizeaza",
            "default model",
            "chosen model",
            "foloseste",
            "uses gguf",
            "works with gguf",
        ],
    ) {
        return MemoryCategory::Decision;
    }
    if contains_any(
        &lowered,
        &[
            "gnomef-rs",
            "gnomef.py",
            "rust port",
            "whatsapp bridge",
            "repo",
            "repository",
            "project",
            "workspace",
            "axum",
            "llama-server",
        ],
    ) {
        return MemoryCategory::ProjectInfo;
    }
    if contains_any(
        &lowered,
        &[
            "wants to build",
            "wants to port",
            "wants to create",
            "wants to integrate",
            "goal",
            "port to rust",
            "implement cross-conversation memory",
        ],
    ) {
        return MemoryCategory::Goal;
    }
    fallback.clone()
}

fn is_persistent_memory_fact(fact: &str, category: &MemoryCategory) -> bool {
    let lowered = normalize_for_match(fact);
    if fact.contains("```")
        || fact.contains('\n')
        || fact.contains('/')
        || contains_any(
            &lowered,
            &[
                ".rs",
                ".py",
                ".js",
                ".ts",
                "src directory",
                "folderul",
                "directorul",
            ],
        )
    {
        return false;
    }
    if contains_any(
        &lowered,
        &[
            "user wants the assistant to",
            "wants tool call improvements",
            "analyze their code",
            "study the code",
            "understand how it works",
            "actively developing a",
            "working on a project with a src",
            "project includes a",
            "software developer working on ai orchestration",
            "local ai orchestration service that runs on port",
            "workflow involving loading",
        ],
    ) {
        return false;
    }

    match category {
        MemoryCategory::UserPreference => contains_any(
            &lowered,
            &[
                "prefer",
                "prefers",
                "romanian",
                "romana",
                "language",
                "limba",
                "response style",
                "does not use",
                "nu foloseste",
                "nu utilizeaza",
            ],
        ),
        MemoryCategory::PersonalInfo => contains_any(
            &lowered,
            &[
                "intel",
                "amd",
                "nvidia",
                "rtx",
                "gtx",
                "cpu",
                "gpu",
                "ram",
                "vram",
                "linux",
                "windows",
                "pcie",
                "ssd",
                "hdd",
                "hardware",
                "procesor",
                "placa video",
            ],
        ),
        MemoryCategory::ProjectInfo => contains_any(
            &lowered,
            &[
                "gnomef-rs",
                "gnomef.py",
                "rust port",
                "whatsapp bridge",
                "repo",
                "repository",
                "axum",
                "llama-server",
                "gguf",
            ],
        ),
        MemoryCategory::Goal => contains_any(
            &lowered,
            &[
                "wants to port",
                "wants to build",
                "wants to create",
                "wants to integrate",
                "port to rust",
            ],
        ),
        MemoryCategory::Decision => contains_any(
            &lowered,
            &[
                "does not use",
                "nu foloseste",
                "nu utilizeaza",
                "default model",
                "uses gguf",
                "works with gguf",
            ],
        ),
    }
}

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

fn extract_json_object(text: &str) -> Option<String> {
    if serde_json::from_str::<Value>(text.trim()).is_ok() {
        return Some(text.trim().to_string());
    }
    let bytes = text.as_bytes();
    let mut start = None;
    let mut depth = 0_i32;
    let mut in_string = false;
    let mut escaped = false;
    for (index, byte) in bytes.iter().enumerate() {
        let ch = *byte as char;
        if in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }
        match ch {
            '"' => in_string = true,
            '{' => {
                if start.is_none() {
                    start = Some(index);
                }
                depth += 1;
            }
            '}' => {
                depth -= 1;
                if depth == 0 {
                    if let Some(start) = start {
                        let candidate = &text[start..=index];
                        if serde_json::from_str::<Value>(candidate).is_ok() {
                            return Some(candidate.to_string());
                        }
                    }
                }
            }
            _ => {}
        }
    }
    None
}

fn dedupe_facts(facts: &mut Vec<MemoryFact>) {
    let mut best_by_key: HashMap<String, MemoryFact> = HashMap::new();
    for fact in facts.drain(..) {
        let key = fact_dedupe_key(&fact.fact);
        match best_by_key.get(&key) {
            Some(existing) if existing.confidence >= fact.confidence => {}
            _ => {
                best_by_key.insert(key, fact);
            }
        }
    }
    let mut values = best_by_key.into_values().collect::<Vec<_>>();
    values.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.last_accessed.cmp(&a.last_accessed))
    });
    *facts = values;
}

fn find_similar_fact(
    facts: &[MemoryFact],
    new_fact: &str,
    category: Option<&MemoryCategory>,
) -> Option<usize> {
    let new_terms = fact_terms(new_fact).into_iter().collect::<HashSet<_>>();
    let new_key = fact_dedupe_key(new_fact);
    facts.iter().enumerate().find_map(|(index, fact)| {
        if category.is_some_and(|category| &fact.category != category) {
            return None;
        }
        if fact_dedupe_key(&fact.fact) == new_key {
            return Some(index);
        }
        if normalize_ws(&fact.fact).eq_ignore_ascii_case(new_fact) {
            return Some(index);
        }
        let old_terms = fact_terms(&fact.fact).into_iter().collect::<HashSet<_>>();
        let overlap = new_terms.intersection(&old_terms).count();
        let union = new_terms.union(&old_terms).count().max(1);
        let jaccard = overlap as f32 / union as f32;
        (jaccard >= 0.66).then_some(index)
    })
}

fn combined_terms(query: &str, history: &[ChatMessage]) -> HashSet<String> {
    let mut combined = query.to_string();
    for message in history.iter().rev().take(4) {
        if message.role != "user" {
            continue;
        }
        let text = message_to_text(message);
        if !text.is_empty() {
            combined.push(' ');
            combined.push_str(&text);
        }
    }
    tokenize_terms(&combined).into_iter().collect()
}

fn tokenize_terms(text: &str) -> Vec<String> {
    let stopwords = [
        "the", "and", "for", "with", "this", "that", "your", "from", "have", "has", "are", "but",
        "into", "about", "just", "was", "were", "will", "would", "should", "could", "te", "rog",
        "sunt", "este", "asta", "acest", "aceasta", "pentru", "despre", "care", "sau", "iar",
        "din", "cu", "pe", "la", "ale", "lui", "cea", "cel", "ce", "cum",
    ]
    .into_iter()
    .collect::<HashSet<_>>();
    text.to_lowercase()
        .chars()
        .map(|ch| if ch.is_alphanumeric() { ch } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .filter(|item| item.len() >= 3 && !stopwords.contains(*item))
        .map(str::to_string)
        .collect::<Vec<_>>()
}

fn fact_terms(text: &str) -> Vec<String> {
    let ignored = [
        "user",
        "assistant",
        "their",
        "works",
        "working",
        "uses",
        "using",
        "local",
        "machine",
        "chat",
        "conversation",
        "future",
        "reference",
        "project",
        "current",
        "active",
    ]
    .into_iter()
    .collect::<HashSet<_>>();
    normalize_for_match(text)
        .split_whitespace()
        .filter(|item| item.len() >= 2 && !ignored.contains(*item))
        .map(singularize_term)
        .collect::<Vec<_>>()
}

fn fact_dedupe_key(text: &str) -> String {
    let mut terms = fact_terms(text);
    terms.sort();
    terms.dedup();
    if terms.is_empty() {
        normalize_for_match(text)
    } else {
        terms.join("|")
    }
}

fn singularize_term(term: &str) -> String {
    if term.len() > 4 && term.ends_with('s') {
        term[..term.len() - 1].to_string()
    } else {
        term.to_string()
    }
}

fn normalize_for_match(text: &str) -> String {
    text.chars()
        .map(deaccent_char)
        .collect::<String>()
        .to_lowercase()
}

fn deaccent_char(ch: char) -> char {
    match ch {
        '\u{0103}' | '\u{00e2}' | '\u{00e1}' | '\u{00e0}' | '\u{00e4}' => 'a',
        '\u{0102}' | '\u{00c2}' | '\u{00c1}' | '\u{00c0}' | '\u{00c4}' => 'a',
        '\u{00ee}' | '\u{00ed}' | '\u{00ec}' | '\u{00ef}' => 'i',
        '\u{00ce}' | '\u{00cd}' | '\u{00cc}' | '\u{00cf}' => 'i',
        '\u{0219}' | '\u{015f}' => 's',
        '\u{0218}' | '\u{015e}' => 's',
        '\u{021b}' | '\u{0163}' => 't',
        '\u{021a}' | '\u{0162}' => 't',
        _ => ch,
    }
}

fn normalize_ws(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn preview(text: &str, max_chars: usize) -> String {
    let chars = text.chars().collect::<Vec<_>>();
    if chars.len() <= max_chars {
        normalize_ws(text)
    } else {
        format!(
            "{}...",
            chars[..max_chars.saturating_sub(3)]
                .iter()
                .collect::<String>()
                .trim()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use std::os::unix::fs::PermissionsExt;

    fn sample_chat() -> Chat {
        Chat {
            id: "chat_001".into(),
            title: "Rust Memory".into(),
            created: Utc::now(),
            messages: vec![
                ChatMessage {
                    role: "user".into(),
                    content: Value::String("Prefer Romanian and concise responses.".into()),
                    timestamp: Utc::now(),
                    extra: Default::default(),
                },
                ChatMessage {
                    role: "assistant".into(),
                    content: Value::String("Noted.".into()),
                    timestamp: Utc::now(),
                    extra: Default::default(),
                },
                ChatMessage {
                    role: "user".into(),
                    content: Value::String("We are porting gnomef.py to Rust.".into()),
                    timestamp: Utc::now(),
                    extra: Default::default(),
                },
            ],
            extra: Default::default(),
        }
    }

    #[test]
    fn recent_summary_is_created_from_chat() {
        let chat = sample_chat();
        let summary = summarize_chat(&chat);
        assert_eq!(summary.chat_id, "chat_001");
        assert!(summary.snippet.contains("Prefer Romanian"));
        assert!(summary.keywords.iter().any(|item| item == "romanian"));
    }

    #[test]
    fn extraction_result_updates_and_forgets_facts() {
        let mut store = MemoryStore {
            facts: vec![MemoryFact {
                id: "mem_1".into(),
                fact: "User prefers English".into(),
                confidence: 0.7,
                source_chat_id: "chat_old".into(),
                timestamp: Utc::now(),
                last_accessed: Utc::now(),
                access_count: 0,
                category: MemoryCategory::UserPreference,
            }],
            recent_conversations: vec![],
        };
        store.apply_extraction_result(
            &AppConfig::default(),
            "chat_001",
            MemoryExtractionResult {
                new_facts: vec![ExtractedFactCandidate {
                    fact: "User prefers Romanian".into(),
                    confidence: 0.95,
                    category: MemoryCategory::UserPreference,
                }],
                updated_facts: vec![],
                facts_to_forget: vec!["mem_1".into()],
            },
        );
        assert_eq!(store.facts.len(), 1);
        assert_eq!(store.facts[0].fact, "User prefers Romanian");
    }

    #[test]
    fn working_memory_prefers_relevant_facts() {
        let now = Utc::now();
        let mut store = MemoryStore {
            facts: vec![
                MemoryFact {
                    id: "mem_1".into(),
                    fact: "User prefers Romanian responses".into(),
                    confidence: 0.99,
                    source_chat_id: "chat_001".into(),
                    timestamp: now,
                    last_accessed: now,
                    access_count: 1,
                    category: MemoryCategory::UserPreference,
                },
                MemoryFact {
                    id: "mem_2".into(),
                    fact: "Project gnomef-rs is a Rust port of gnomef.py".into(),
                    confidence: 0.97,
                    source_chat_id: "chat_002".into(),
                    timestamp: now,
                    last_accessed: now,
                    access_count: 1,
                    category: MemoryCategory::ProjectInfo,
                },
            ],
            recent_conversations: vec![RecentConversationSummary {
                chat_id: "chat_003".into(),
                title: "WhatsApp bridge".into(),
                snippet: "Diagnosed WhatsApp bridge reply path.".into(),
                updated_at: now,
                keywords: vec!["whatsapp".into(), "bridge".into(), "reply".into()],
            }],
        };
        let snapshot = store.build_working_memory(
            &AppConfig::default(),
            "Continue Rust port work",
            &sample_chat().messages,
            Some("chat_004"),
        );
        assert!(
            snapshot
                .facts
                .iter()
                .any(|fact| fact.fact.contains("Rust port of gnomef.py"))
        );
        assert!(
            snapshot
                .prompt_block()
                .contains("Cross-Conversation Memory")
        );
    }

    #[test]
    fn working_memory_filters_facts_and_chats_by_age() {
        let now = Utc::now();
        let old = now - ChronoDuration::days(120);
        let mut store = MemoryStore {
            facts: vec![
                MemoryFact {
                    id: "recent".into(),
                    fact: "User prefers Romanian responses".into(),
                    confidence: 0.99,
                    source_chat_id: "chat_recent".into(),
                    timestamp: now,
                    last_accessed: now,
                    access_count: 0,
                    category: MemoryCategory::UserPreference,
                },
                MemoryFact {
                    id: "old".into(),
                    fact: "User prefers very old English responses".into(),
                    confidence: 0.99,
                    source_chat_id: "chat_old".into(),
                    timestamp: old,
                    last_accessed: old,
                    access_count: 0,
                    category: MemoryCategory::UserPreference,
                },
            ],
            recent_conversations: vec![RecentConversationSummary {
                chat_id: "chat_old".into(),
                title: "Old preference".into(),
                snippet: "Discussed very old English responses.".into(),
                updated_at: old,
                keywords: vec!["responses".into()],
            }],
        };
        let mut cfg = AppConfig::default();
        cfg.memory_max_age_days = 30;
        let snapshot = store.build_working_memory(&cfg, "responses", &[], Some("chat_current"));

        assert_eq!(snapshot.facts.len(), 1);
        assert_eq!(snapshot.facts[0].id, "recent");
        assert!(snapshot.recent_conversations.is_empty());
        assert!(snapshot.prompt_block().contains("last 30 days"));
    }

    #[test]
    fn parses_extraction_json_from_markdown_wrapper() {
        let text = "```json\n{\"new_facts\":[{\"fact\":\"User prefers Romanian\",\"confidence\":0.95,\"category\":\"UserPreference\"}],\"updated_facts\":[],\"facts_to_forget\":[]}\n```";
        let parsed = parse_extraction_response(text).unwrap();
        assert_eq!(parsed.new_facts.len(), 1);
        assert_eq!(parsed.new_facts[0].category, MemoryCategory::UserPreference);
    }

    #[test]
    fn append_memory_block_keeps_base_prompt_when_empty() {
        assert_eq!(append_memory_block("Base prompt", None), "Base prompt");
        assert_eq!(
            append_memory_block("Base prompt", Some("  ")),
            "Base prompt"
        );
    }

    #[test]
    fn append_memory_block_includes_cross_conversation_memory() {
        let merged = append_memory_block("Base prompt", Some("Cross-Conversation Memory\n- fact"));
        assert!(merged.contains("Base prompt"));
        assert!(merged.contains("Cross-Conversation Memory"));
        assert!(merged.contains("- fact"));
    }

    #[test]
    fn working_memory_includes_related_recent_conversations() {
        let now = Utc::now();
        let mut store = MemoryStore {
            facts: vec![],
            recent_conversations: vec![
                RecentConversationSummary {
                    chat_id: "chat_rust".into(),
                    title: "Rust port".into(),
                    snippet: "Discussed Rust port architecture and axum backend.".into(),
                    updated_at: now,
                    keywords: vec!["rust".into(), "backend".into(), "axum".into()],
                },
                RecentConversationSummary {
                    chat_id: "chat_other".into(),
                    title: "Unrelated".into(),
                    snippet: "Weather planning for tomorrow.".into(),
                    updated_at: now,
                    keywords: vec!["weather".into()],
                },
            ],
        };
        let mut cfg = AppConfig::default();
        cfg.memory_max_recent_summaries_in_prompt = 1;
        let snapshot = store.build_working_memory(
            &cfg,
            "continue the Rust backend work",
            &sample_chat().messages,
            Some("chat_new"),
        );
        assert_eq!(snapshot.recent_conversations.len(), 1);
        assert_eq!(snapshot.recent_conversations[0].chat_id, "chat_rust");
    }

    #[test]
    fn memory_store_round_trips_to_disk() {
        let tmp = std::env::temp_dir().join(format!("gnomef_memory_test_{}", Uuid::new_v4()));
        let paths = AppPaths::new(tmp.clone()).unwrap();
        let now = Utc::now();
        let store = MemoryStore {
            facts: vec![MemoryFact {
                id: "mem_1".into(),
                fact: "User prefers Romanian responses".into(),
                confidence: 0.95,
                source_chat_id: "chat_001".into(),
                timestamp: now,
                last_accessed: now,
                access_count: 2,
                category: MemoryCategory::UserPreference,
            }],
            recent_conversations: vec![RecentConversationSummary {
                chat_id: "chat_001".into(),
                title: "Memory".into(),
                snippet: "Persistent preferences and setup.".into(),
                updated_at: now,
                keywords: vec!["persistent".into(), "preferences".into()],
            }],
        };
        store.save(&paths).unwrap();
        assert_eq!(
            fs::metadata(&paths.memory_store_file)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let loaded = MemoryStore::load(&paths).unwrap();
        assert_eq!(loaded.facts.len(), 1);
        assert_eq!(loaded.recent_conversations.len(), 1);
        assert_eq!(loaded.facts[0].fact, "User prefers Romanian responses");
        let _ = fs::remove_dir_all(tmp);
    }

    #[test]
    fn normalize_filters_noisy_code_and_task_facts() {
        let mut store = MemoryStore {
            facts: vec![
                MemoryFact {
                    id: "mem_1".into(),
                    fact: "User's gnomef-rs project includes a vision module (vision.rs) for visual processing".into(),
                    confidence: 0.9,
                    source_chat_id: "chat_001".into(),
                    timestamp: Utc::now(),
                    last_accessed: Utc::now(),
                    access_count: 0,
                    category: MemoryCategory::ProjectInfo,
                },
                MemoryFact {
                    id: "mem_2".into(),
                    fact: "User prefers Romanian language for communication".into(),
                    confidence: 0.95,
                    source_chat_id: "chat_001".into(),
                    timestamp: Utc::now(),
                    last_accessed: Utc::now(),
                    access_count: 0,
                    category: MemoryCategory::UserPreference,
                },
            ],
            recent_conversations: vec![],
        };
        store.normalize();
        assert_eq!(store.facts.len(), 1);
        assert_eq!(store.facts[0].category, MemoryCategory::UserPreference);
    }

    #[test]
    fn normalize_recategorizes_hardware_as_personal_info() {
        let mut store = MemoryStore {
            facts: vec![MemoryFact {
                id: "mem_1".into(),
                fact: "User has an Intel Core i5-6600K processor and NVIDIA RTX 4070 GPU".into(),
                confidence: 0.98,
                source_chat_id: "chat_001".into(),
                timestamp: Utc::now(),
                last_accessed: Utc::now(),
                access_count: 0,
                category: MemoryCategory::UserPreference,
            }],
            recent_conversations: vec![],
        };
        store.normalize();
        assert_eq!(store.facts.len(), 1);
        assert_eq!(store.facts[0].category, MemoryCategory::PersonalInfo);
    }

    #[test]
    fn normalize_dedupes_semantic_gguf_variants() {
        let now = Utc::now();
        let mut store = MemoryStore {
            facts: vec![
                MemoryFact {
                    id: "mem_1".into(),
                    fact: "User works with GGUF format AI models for inference".into(),
                    confidence: 0.95,
                    source_chat_id: "chat_001".into(),
                    timestamp: now,
                    last_accessed: now,
                    access_count: 0,
                    category: MemoryCategory::Decision,
                },
                MemoryFact {
                    id: "mem_2".into(),
                    fact: "User works with GGUF model format for AI inference".into(),
                    confidence: 0.94,
                    source_chat_id: "chat_002".into(),
                    timestamp: now,
                    last_accessed: now,
                    access_count: 0,
                    category: MemoryCategory::Decision,
                },
            ],
            recent_conversations: vec![],
        };
        store.normalize();
        assert_eq!(store.facts.len(), 1);
    }

    #[test]
    fn secrets_never_survive_normalization() {
        assert!(looks_like_secret(
            "User's OpenAI api key is sk-abc123def456ghi789"
        ));
        assert!(looks_like_secret("parola contului este hunter2secret"));
        assert!(looks_like_secret(
            "GitHub token ghp_16C7e42F292c6912E7710c838347Ae178B4a"
        ));
        assert!(looks_like_secret(
            "Uses the value Ab3dEf6hIj9kLm2nOp5qRs8tUv1wXy4zAb3dEf6h somewhere"
        ));
        assert!(!looks_like_secret("User prefers Romanian answers"));
        assert!(!looks_like_secret(
            "User's model has a 4096 token context window limit"
        ));

        let now = Utc::now();
        let mut store = MemoryStore {
            facts: vec![MemoryFact {
                id: "mem_secret".into(),
                fact: "User's Anthropic api key is sk-ant-xxxxxxxxxxxxxxxx".into(),
                confidence: 0.99,
                source_chat_id: "chat_001".into(),
                timestamp: now,
                last_accessed: now,
                access_count: 0,
                category: MemoryCategory::PersonalInfo,
            }],
            recent_conversations: vec![],
        };
        store.normalize();
        assert!(store.facts.is_empty());
    }

    #[test]
    fn secrets_never_enter_recent_conversation_summaries() {
        let mut chat = sample_chat();
        chat.title = "API key sk-ant-title-secret-123456789".into();
        chat.messages.push(ChatMessage {
            role: "user".into(),
            content: Value::String(
                "My Anthropic API key is sk-ant-message-secret-123456789".into(),
            ),
            timestamp: Utc::now(),
            extra: Default::default(),
        });

        let summary = summarize_chat(&chat);
        assert_eq!(summary.title, "Conversation");
        assert!(!summary.snippet.contains("sk-ant"));

        let mut store = MemoryStore {
            facts: vec![],
            recent_conversations: vec![RecentConversationSummary {
                chat_id: "chat_leaky".into(),
                title: "Normal title".into(),
                snippet: "password is hunter2secret".into(),
                updated_at: Utc::now(),
                keywords: vec!["password".into()],
            }],
        };
        store.normalize();
        assert!(store.recent_conversations.is_empty());
    }
}
