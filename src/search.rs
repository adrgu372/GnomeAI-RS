use regex::Regex;
use serde_json::Value;

use crate::storage::ChatMessage;

#[derive(Debug, Clone)]
pub struct SearchDecision {
    pub search: bool,
    pub query: String,
    pub reason: String,
    pub score: i32,
}

pub fn should_auto_search(msg: &str, history: &[ChatMessage]) -> SearchDecision {
    let original = normalize_ws(msg);
    let raw_query = clean_search_query(msg);
    if raw_query.is_empty() {
        return decision(false, "", "empty", 0);
    }

    let match_query = normalize_for_match(&raw_query);
    if matches_any(
        &[
            r"^(hi|hello|hey|thanks|thank you|ok|okay|yo)\b",
            r"^(salut|buna|mersi|multumesc|ok|bine)\b",
        ],
        &match_query,
    ) {
        return decision(false, &raw_query, "chat", 0);
    }
    if should_skip(&match_query) {
        return decision(false, &raw_query, "skip", 0);
    }
    if is_local_runtime_query(&match_query, history) {
        return decision(false, &raw_query, "local-runtime", 0);
    }

    let expanded = expand_query_with_context(&raw_query, history);
    let expanded_match = normalize_for_match(&expanded);
    if is_local_runtime_query(&expanded_match, history) {
        return decision(false, &expanded, "local-runtime", 0);
    }
    let is_question = original.ends_with('?');
    let named_entity = Regex::new(r"\b[A-Z][a-z]+(?:\s+[A-Z][a-z]+){1,3}\b")
        .unwrap()
        .is_match(&expanded);
    let profile_request = matches_any(
        &[
            r"\b(tell me about|about|background|bio|biography|profile)\b",
            r"\b(despre|spune-mi despre|biografie|profil|cine este)\b",
        ],
        &expanded_match,
    );
    let relation_request = matches_any(
        &[
            r"\b(wife|husband|partner|family|children|son|daughter|age|net worth|married)\b",
            r"\b(sotie|sot|partener|familie|copii|fiu|fiica|varsta|avere|casatorit)\b",
        ],
        &expanded_match,
    );

    let mut score = 0;
    let mut reasons = Vec::new();

    if matches_any(
        &[
            r"\b(latest|current|today|now|recent|recently|new|news|update|updated|live|this year|202[0-9]|20[3-9][0-9])\b",
            r"\b(acum|azi|astazi|curent|actual|actualul|actuala|recent|noutati|ultima|ultimul|ultimele|ultime|anul acesta)\b",
        ],
        &expanded_match,
    ) {
        score += 4;
        reasons.push("current");
    }
    if matches_any(
        &[
            r"\b(price|stock|weather|forecast|score|result|schedule|release|version|ceo|founder|president|prime minister|mayor|minister|phone|address|hours|website|official site|location|map)\b",
            r"\b(pret|vreme|scor|rezultat|program|orar|lansare|versiune|prim-ministru|prim ministru|premier|primar|presedinte|ministru|telefon|adresa|site|locatie|harta)\b",
        ],
        &expanded_match,
    ) {
        score += 3;
        reasons.push("dynamic");
    }
    if matches_any(
        &[
            r"\b(search|find|look up|browse|check|verify|source)\b",
            r"\b(cauta|gaseste|verifica|uita-te|uitate|cauta pe net)\b",
        ],
        &expanded_match,
    ) {
        score += 2;
        reasons.push("lookup");
    }
    if matches_any(
        &[
            r"\b(who|what|when|where|which|whose|how many|how much)\b",
            r"\b(cine|ce|cand|unde|care|cat|cate)\b",
        ],
        &expanded_match,
    ) {
        score += 2;
        reasons.push("factual");
    }
    if profile_request
        && (named_entity
            || matches_any(
                &[r"^(tell me about|spune-mi despre|despre)\s+\S+\s+\S+"],
                &expanded_match,
            ))
    {
        score += 3;
        reasons.push("profile");
    }
    if relation_request {
        score += 3;
        reasons.push("relation");
    }
    if is_question {
        score += 1;
        reasons.push("question");
    }
    let words = expanded.split_whitespace().count();
    if (2..=18).contains(&words) {
        score += 1;
        reasons.push("compact");
    }
    if matches_any(&[r"\b(of|in|for|near|din|despre|langa)\b"], &expanded_match) {
        score += 1;
        reasons.push("entity");
    }
    if named_entity {
        score += 1;
        reasons.push("named");
    }

    if matches_any(
        &[
            r"^(what is|explain|how does)\s+[a-z0-9 _-]{1,40}$",
            r"^(ce este|explica|cum functioneaza)\s+[a-z0-9 _-]{1,40}$",
        ],
        &expanded_match,
    ) && score < 4
    {
        return decision(false, &expanded, "concept", score);
    }

    let query = optimize_search_query(&expanded);
    SearchDecision {
        search: score >= 3,
        query,
        reason: if reasons.is_empty() {
            "low-score".into()
        } else {
            reasons.join(",")
        },
        score,
    }
}

pub fn is_release_query(query: &str) -> bool {
    let lowered = normalize_for_match(query);
    [
        "version",
        "release",
        "download",
        "update",
        "installer",
        "changelog",
        "versiune",
        "lansare",
        "descarc",
        "actualizare",
    ]
    .iter()
    .any(|needle| lowered.contains(needle))
}

fn decision(search: bool, query: &str, reason: &str, score: i32) -> SearchDecision {
    SearchDecision {
        search,
        query: query.to_string(),
        reason: reason.into(),
        score,
    }
}

fn clean_search_query(text: &str) -> String {
    let cleaned = normalize_ws(text);
    let normalized = normalize_for_match(&cleaned);
    let stripped = Regex::new(
        r"^(please|pls|te rog|poti sa|spune-mi|zi-mi|vreau sa stiu|can you|could you|would you)\s+",
    )
    .unwrap()
    .replace(&normalized, "")
    .to_string();
    Regex::new(r"^(search|find|look up|browse|check|verify|cauta|gaseste|verifica)\s+")
        .unwrap()
        .replace(&stripped, "")
        .trim_matches(|ch: char| ch.is_whitespace() || "?.!".contains(ch))
        .to_string()
}

fn recent_user_messages(history: &[ChatMessage], limit: usize) -> Vec<String> {
    let mut items = Vec::new();
    for message in history.iter().rev() {
        if message.role != "user" {
            continue;
        }
        let Value::String(text) = &message.content else {
            continue;
        };
        let cleaned = clean_search_query(text);
        if cleaned.is_empty() {
            continue;
        }
        items.push(cleaned);
        if items.len() >= limit {
            break;
        }
    }
    items.reverse();
    items
}

fn expand_query_with_context(query: &str, history: &[ChatMessage]) -> String {
    let query_match = normalize_for_match(query);
    if !matches_any(
        &[
            r"\b(it|this|that|he|she|they|them|him|her|his|hers|its|their|asta|acesta|aceasta|el|ea|ei|ele|lor|lui)\b",
        ],
        &query_match,
    ) {
        return query.to_string();
    }

    for previous in recent_user_messages(history, 5).into_iter().rev() {
        if previous == query {
            continue;
        }
        let previous_match = normalize_for_match(&previous);
        if matches_any(
            &[
                r"^(hi|hello|hey|thanks|thank you|ok|okay|yo)\b",
                r"^(salut|buna|mersi|multumesc|ok|bine)\b",
            ],
            &previous_match,
        ) {
            continue;
        }
        if should_skip(&previous_match) {
            continue;
        }
        return normalize_ws(&format!("{previous} {query}"));
    }
    query.to_string()
}

fn optimize_search_query(query: &str) -> String {
    let cleaned = normalize_ws(query);
    let lowered = normalize_for_match(&cleaned);
    if is_release_query(&cleaned) {
        if !lowered.contains("official") && !lowered.contains("release") {
            return format!("{cleaned} official release");
        }
        if !lowered.contains("official") {
            return format!("{cleaned} official");
        }
    }
    cleaned
}

fn should_skip(text: &str) -> bool {
    text.contains("```")
        || matches_any(
            &[
                r"\b(traceback|stack trace|exception|segmentation fault)\b",
                r"\b(write code|debug|fix|refactor|implement|function|class|variable|regex|query plan|endpoint|unit test|stack trace)\b",
                r"\b(scrie cod|debug|repara|refactorizeaza|implementeaza|functie|clasa|variabila|regex|endpoint|test unitar)\b",
                r"\b(write|draft|rewrite|translate|summarize|brainstorm|poem|story)\b",
                r"\b(scrie|redacteaza|rescrie|tradu|rezuma|brainstorm|poezie|poveste)\b",
                r"\b(def|class|import|from|select|insert|update|delete|curl|pip|npm)\b",
            ],
            text,
        )
}

fn is_local_runtime_query(text: &str, history: &[ChatMessage]) -> bool {
    let local_subject = matches_any(
        &[
            r"^(cpu|gpu|ram|ssd|hdd|vram|pc|host|runtime)$",
            r"\b(cpu|gpu|ram|ssd|hdd|vram|motherboard|bios|temperatura|temperature|fan|vram|nvidia-smi|lscpu|lspci|uname|hostname)\b",
            r"\b(procesor|placa video|placa grafic[ăa]|placa de baza|memorie|temperatura gpu|temperatura cpu)\b",
            r"\b(workspace|folder|director|fisier|fișier|repo|repository|codul meu|proiectul meu|src)\b",
            r"\b(pc-ul meu|pcul meu|calculatorul meu|sistemul meu|masina mea|mașina mea|hostul meu)\b",
            r"\b(rulezi|ruleaza|rulati|rulam|executi|executa)\b",
        ],
        text,
    );
    if !local_subject {
        return false;
    }

    if matches_any(
        &[
            r"\b(current|latest|today|news|release|version|official|price|stock|weather|president)\b",
            r"\b(actual|ultima|ultimul|noutati|știri|stiri|pret|vreme|presedinte|versiune)\b",
        ],
        text,
    ) {
        return false;
    }

    if text.split_whitespace().count() <= 4 {
        return true;
    }

    let recent = recent_user_messages(history, 4).join(" ");
    let recent = normalize_for_match(&recent);
    matches_any(
        &[
            r"\b(componente|hardware|specs|specificatii|specificații|runtime|local|workspace|gazda|host)\b",
            r"\b(pc-ul meu|pcul meu|calculatorul meu|sistemul meu|masina mea|mașina mea)\b",
            r"\b(cpu|gpu|ram|ssd|procesor|placa video|placa grafic[ăa])\b",
            r"\b(citeste codul|studiaza codul|analizeaza codul|src|workspace)\b",
        ],
        &recent,
    )
}

fn matches_any(patterns: &[&str], text: &str) -> bool {
    patterns.iter().any(|pattern| {
        Regex::new(&format!("(?i){pattern}"))
            .map(|re| re.is_match(text))
            .unwrap_or(false)
    })
}

fn normalize_ws(text: &str) -> String {
    Regex::new(r"\s+")
        .unwrap()
        .replace_all(text, " ")
        .trim()
        .to_string()
}

fn normalize_for_match(text: &str) -> String {
    normalize_ws(
        &text
            .chars()
            .map(deaccent_char)
            .collect::<String>()
            .to_lowercase(),
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::Value;

    fn user_message(text: &str) -> ChatMessage {
        ChatMessage {
            role: "user".into(),
            content: Value::String(text.into()),
            timestamp: Utc::now(),
            extra: Default::default(),
        }
    }

    #[test]
    fn skips_auto_search_for_local_gpu_question() {
        let history = vec![
            user_message("Te rog să identifici componentele PC-ului pe care rulezi"),
            user_message("Ok, ce CPU am pe PC?"),
        ];
        let decision = should_auto_search("GPU?", &history);
        assert!(!decision.search);
        assert_eq!(decision.reason, "local-runtime");
    }

    #[test]
    fn skips_auto_search_for_local_workspace_question() {
        let history = vec![user_message(
            "Studiază te rog directorul src din folderul actual",
        )];
        let decision = should_auto_search("Ce e în workspace-ul meu?", &history);
        assert!(!decision.search);
        assert_eq!(decision.reason, "local-runtime");
    }

    #[test]
    fn keeps_auto_search_for_public_entity_question() {
        let history = vec![];
        let decision = should_auto_search("Caută ultimele info despre Claude Mythos", &history);
        assert!(decision.search);
    }
}
