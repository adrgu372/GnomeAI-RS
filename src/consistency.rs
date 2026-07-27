use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::runtime_profile::{RuntimeModeKind, RuntimeProfile, SourceAttribution};

#[derive(Debug, Clone, Serialize)]
pub struct ToolObservation {
    pub tool_name: String,
    pub success: bool,
    pub source_attribution: SourceAttribution,
    pub summary: String,
    pub capability_tags: Vec<String>,
    pub raw_result: Value,
}

impl ToolObservation {
    pub fn from_success(
        profile: &RuntimeProfile,
        tool_name: &str,
        args: &Map<String, Value>,
        result: &Value,
    ) -> Self {
        let source_attribution = SourceAttribution::for_tool(profile, tool_name, args);
        let capability_tags = collect_capability_tags(tool_name, args, true, &source_attribution);
        let summary = summarize_tool_result(tool_name, args, result, true, &source_attribution);
        Self {
            tool_name: tool_name.into(),
            success: true,
            source_attribution,
            summary,
            capability_tags,
            raw_result: result.clone(),
        }
    }

    pub fn from_error(
        profile: &RuntimeProfile,
        tool_name: &str,
        args: &Map<String, Value>,
        error: &str,
    ) -> Self {
        let source_attribution = SourceAttribution::for_tool(profile, tool_name, args);
        let capability_tags = collect_capability_tags(tool_name, args, false, &source_attribution);
        let raw_result = json!({"error": error});
        let summary =
            summarize_tool_result(tool_name, args, &raw_result, false, &source_attribution);
        Self {
            tool_name: tool_name.into(),
            success: false,
            source_attribution,
            summary,
            capability_tags,
            raw_result,
        }
    }

    pub fn as_json_for_model(&self) -> Value {
        json!({
            "tool_name": self.tool_name,
            "success": self.success,
            "summary": self.summary,
            "capability_tags": self.capability_tags,
            "source_attribution": self.source_attribution.as_json(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsistencyViolationKind {
    ClaimsRemoteRuntimeForLocalSession,
    ClaimsLocalRuntimeForNonLocalSession,
    DeniesHardwareAccessDespiteLocalAccess,
    DeniesFileAccessDespiteLocalAccess,
    ClaimsExternalWebAsLocalVerification,
    ClaimsRemoteEvidenceAsUserDeviceFact,
    ClaimsUserDeviceInspectionWithoutBridge,
}

#[derive(Debug, Clone)]
pub struct ConsistencyViolation {
    pub kind: ConsistencyViolationKind,
}

#[derive(Debug, Default)]
struct EvidenceSummary {
    runtime_kind: Option<RuntimeModeKind>,
    runtime_allows_user_hardware: bool,
    runtime_allows_user_workspace: bool,
    any_user_device_evidence: bool,
    any_local_hardware_evidence: bool,
    any_local_workspace_evidence: bool,
    any_external_web_evidence: bool,
    any_remote_evidence: bool,
    any_container_evidence: bool,
}

impl EvidenceSummary {
    fn from(profile: &RuntimeProfile, observations: &[ToolObservation]) -> Self {
        let mut summary = Self {
            runtime_kind: Some(profile.mode_kind()),
            runtime_allows_user_hardware: profile.can_directly_inspect_user_hardware(),
            runtime_allows_user_workspace: profile.can_directly_inspect_user_workspace(),
            ..Self::default()
        };

        for observation in observations.iter().filter(|item| item.success) {
            if observation.source_attribution.can_infer_user_device {
                summary.any_user_device_evidence = true;
            }
            if observation
                .capability_tags
                .iter()
                .any(|tag| tag == "hardware_inspection")
                && observation.source_attribution.can_infer_user_device
            {
                summary.any_local_hardware_evidence = true;
            }
            if observation
                .capability_tags
                .iter()
                .any(|tag| tag == "workspace_inspection")
                && observation.source_attribution.can_infer_user_device
            {
                summary.any_local_workspace_evidence = true;
            }
            if observation
                .capability_tags
                .iter()
                .any(|tag| tag == "external_web")
            {
                summary.any_external_web_evidence = true;
            }
            if observation
                .capability_tags
                .iter()
                .any(|tag| tag == "remote_execution")
            {
                summary.any_remote_evidence = true;
            }
            if observation
                .capability_tags
                .iter()
                .any(|tag| tag == "container_execution")
            {
                summary.any_container_evidence = true;
            }
        }
        summary
    }

    fn user_device_claims_allowed(&self) -> bool {
        self.runtime_allows_user_hardware
            || self.runtime_allows_user_workspace
            || self.any_user_device_evidence
    }
}

pub fn validate_final_answer(
    answer: &str,
    profile: &RuntimeProfile,
    observations: &[ToolObservation],
) -> Vec<ConsistencyViolation> {
    let normalized = normalize_text(answer);
    let summary = EvidenceSummary::from(profile, observations);
    let mut violations = Vec::new();

    if denies_hardware_access(&normalized)
        && (summary.runtime_allows_user_hardware || summary.any_local_hardware_evidence)
    {
        violations.push(ConsistencyViolation {
            kind: ConsistencyViolationKind::DeniesHardwareAccessDespiteLocalAccess,
        });
    }

    if denies_file_access(&normalized)
        && (summary.runtime_allows_user_workspace || summary.any_local_workspace_evidence)
    {
        violations.push(ConsistencyViolation {
            kind: ConsistencyViolationKind::DeniesFileAccessDespiteLocalAccess,
        });
    }

    if claims_remote_runtime(&normalized)
        && summary.runtime_kind == Some(RuntimeModeKind::LocalUserMachine)
    {
        violations.push(ConsistencyViolation {
            kind: ConsistencyViolationKind::ClaimsRemoteRuntimeForLocalSession,
        });
    }

    if claims_local_runtime_or_user_machine_fact(&normalized)
        && !summary.user_device_claims_allowed()
    {
        violations.push(ConsistencyViolation {
            kind: ConsistencyViolationKind::ClaimsLocalRuntimeForNonLocalSession,
        });
    }

    if claims_user_machine_verification(&normalized)
        && summary.any_external_web_evidence
        && !summary.any_user_device_evidence
    {
        violations.push(ConsistencyViolation {
            kind: ConsistencyViolationKind::ClaimsExternalWebAsLocalVerification,
        });
    }

    if claims_user_machine_verification(&normalized)
        && (summary.any_remote_evidence || summary.any_container_evidence)
        && !summary.any_user_device_evidence
    {
        violations.push(ConsistencyViolation {
            kind: ConsistencyViolationKind::ClaimsRemoteEvidenceAsUserDeviceFact,
        });
    }

    if (claims_user_file_inspection(&normalized) || claims_user_hardware_inspection(&normalized))
        && !summary.user_device_claims_allowed()
    {
        violations.push(ConsistencyViolation {
            kind: ConsistencyViolationKind::ClaimsUserDeviceInspectionWithoutBridge,
        });
    }

    dedupe_violations(violations)
}

pub fn enforce_final_answer(
    answer: &str,
    profile: &RuntimeProfile,
    observations: &[ToolObservation],
) -> String {
    let trimmed = answer.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let violations = validate_final_answer(trimmed, profile, observations);
    if violations.is_empty() {
        return trimmed.to_string();
    }

    let kept = strip_inconsistent_sentences(trimmed, profile, observations);
    let corrections = build_authoritative_corrections(profile, observations, &violations);
    let mut sections = Vec::new();
    if !kept.trim().is_empty() {
        sections.push(kept.trim().to_string());
    }
    if !corrections.is_empty() {
        sections.push(corrections.join("\n"));
    }
    if sections.is_empty() {
        corrections.join("\n")
    } else {
        sections.join("\n\n")
    }
}

fn summarize_tool_result(
    tool_name: &str,
    args: &Map<String, Value>,
    result: &Value,
    success: bool,
    source_attribution: &SourceAttribution,
) -> String {
    if !success {
        return format!(
            "{} failed in {} on host '{}': {}",
            tool_name,
            source_attribution.execution_context,
            source_attribution.host,
            preview_text(
                result
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error"),
                220,
            )
        );
    }

    match tool_name {
        "Bash" => {
            let command = args
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or("unknown command");
            let exit_code = result
                .get("exit_code")
                .and_then(Value::as_i64)
                .unwrap_or_default();
            let stdout = preview_text(
                result.get("stdout").and_then(Value::as_str).unwrap_or(""),
                180,
            );
            let stderr = preview_text(
                result.get("stderr").and_then(Value::as_str).unwrap_or(""),
                140,
            );
            if stderr.is_empty() {
                format!(
                    "Bash ran on '{}' with command `{}` and exit code {}. stdout: {}",
                    source_attribution.host,
                    command,
                    exit_code,
                    if stdout.is_empty() {
                        "[empty]"
                    } else {
                        &stdout
                    }
                )
            } else {
                format!(
                    "Bash ran on '{}' with command `{}` and exit code {}. stdout: {} stderr: {}",
                    source_attribution.host,
                    command,
                    exit_code,
                    if stdout.is_empty() {
                        "[empty]"
                    } else {
                        &stdout
                    },
                    stderr
                )
            }
        }
        "Read" => format!(
            "Read file '{}' from scope '{}' on host '{}'.",
            result
                .get("path")
                .and_then(Value::as_str)
                .unwrap_or("unknown path"),
            source_attribution.source_scope,
            source_attribution.host
        ),
        "Glob" => format!(
            "Glob listed {} path(s) from scope '{}' on host '{}'.",
            result
                .get("matches")
                .and_then(Value::as_array)
                .map(|items| items.len())
                .unwrap_or_default(),
            source_attribution.source_scope,
            source_attribution.host
        ),
        "Grep" => format!(
            "Grep found {} match(es) under scope '{}' on host '{}'.",
            result
                .get("matches")
                .and_then(Value::as_array)
                .map(|items| items.len())
                .unwrap_or_default(),
            source_attribution.source_scope,
            source_attribution.host
        ),
        "WebSearch" => format!(
            "WebSearch used external web sources through '{}' for query '{}'.",
            source_attribution.host,
            result.get("query").and_then(Value::as_str).unwrap_or("")
        ),
        "WebFetch" => format!(
            "WebFetch retrieved external content from '{}' through '{}'.",
            result
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or("unknown url"),
            source_attribution.host
        ),
        "Agent" => format!(
            "Agent produced a result in execution_context='{}' on host '{}'.",
            source_attribution.execution_context, source_attribution.host
        ),
        _ => format!(
            "{} returned data from source_scope='{}' on host '{}'.",
            tool_name, source_attribution.source_scope, source_attribution.host
        ),
    }
}

fn collect_capability_tags(
    tool_name: &str,
    args: &Map<String, Value>,
    success: bool,
    source_attribution: &SourceAttribution,
) -> Vec<String> {
    let mut tags = Vec::new();
    if !success {
        tags.push("tool_error".into());
        return tags;
    }

    if source_attribution.can_infer_user_device {
        tags.push("user_device_fact".into());
    }

    match source_attribution.source_scope.as_str() {
        "local_host" => tags.push("local_host".into()),
        "local_workspace" | "local_filesystem" => tags.push("workspace_inspection".into()),
        "external_web" => tags.push("external_web".into()),
        "remote_host" | "remote_workspace" | "remote_runtime" | "remote_agent_runtime" => {
            tags.push("remote_execution".into())
        }
        "container_host" | "container_workspace" | "container_agent_runtime" => {
            tags.push("container_execution".into())
        }
        _ => {}
    }

    if tool_name == "Bash" {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_lowercase();
        if is_hardware_command(&command) {
            tags.push("hardware_inspection".into());
        }
        if is_gpu_command(&command) {
            tags.push("gpu_inspection".into());
        }
    }

    if matches!(tool_name, "Read" | "Write" | "Edit" | "Glob" | "Grep") {
        tags.push("workspace_inspection".into());
    }

    tags.sort();
    tags.dedup();
    tags
}

fn is_hardware_command(command: &str) -> bool {
    [
        "nvidia-smi",
        "sensors",
        "lscpu",
        "lspci",
        "lsusb",
        "lshw",
        "dmidecode",
        "vcgencmd",
        "/sys/class/thermal",
        "hwinfo",
        "inxi",
        "free -",
        "uname -a",
    ]
    .iter()
    .any(|item| command.contains(item))
}

fn is_gpu_command(command: &str) -> bool {
    ["nvidia-smi", "rocm-smi", "nvtop", "gpu"]
        .iter()
        .any(|item| command.contains(item))
}

fn strip_inconsistent_sentences(
    answer: &str,
    profile: &RuntimeProfile,
    observations: &[ToolObservation],
) -> String {
    let summary = EvidenceSummary::from(profile, observations);
    split_sentences(answer)
        .into_iter()
        .filter(|sentence| {
            let normalized = normalize_text(sentence);
            !sentence_is_inconsistent(&normalized, &summary)
        })
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_string()
}

fn sentence_is_inconsistent(normalized: &str, summary: &EvidenceSummary) -> bool {
    (denies_hardware_access(normalized)
        && (summary.runtime_allows_user_hardware || summary.any_local_hardware_evidence))
        || (denies_file_access(normalized)
            && (summary.runtime_allows_user_workspace || summary.any_local_workspace_evidence))
        || (claims_remote_runtime(normalized)
            && summary.runtime_kind == Some(RuntimeModeKind::LocalUserMachine))
        || (claims_local_runtime_or_user_machine_fact(normalized)
            && !summary.user_device_claims_allowed())
        || (claims_user_machine_verification(normalized)
            && (summary.any_external_web_evidence
                || summary.any_remote_evidence
                || summary.any_container_evidence)
            && !summary.any_user_device_evidence)
}

fn build_authoritative_corrections(
    profile: &RuntimeProfile,
    observations: &[ToolObservation],
    violations: &[ConsistencyViolation],
) -> Vec<String> {
    let mut lines = vec![profile.runtime_context_sentence()];
    let mut needs_hardware_line = false;
    let mut needs_workspace_line = false;
    let mut needs_external_line = false;
    let mut needs_remote_line = false;
    let mut needs_no_bridge_line = false;

    for violation in violations {
        match violation.kind {
            ConsistencyViolationKind::ClaimsRemoteRuntimeForLocalSession
            | ConsistencyViolationKind::DeniesHardwareAccessDespiteLocalAccess => {
                needs_hardware_line = true;
            }
            ConsistencyViolationKind::DeniesFileAccessDespiteLocalAccess => {
                needs_workspace_line = true;
            }
            ConsistencyViolationKind::ClaimsExternalWebAsLocalVerification => {
                needs_external_line = true;
            }
            ConsistencyViolationKind::ClaimsRemoteEvidenceAsUserDeviceFact => {
                needs_remote_line = true;
            }
            ConsistencyViolationKind::ClaimsLocalRuntimeForNonLocalSession
            | ConsistencyViolationKind::ClaimsUserDeviceInspectionWithoutBridge => {
                needs_no_bridge_line = true;
                if profile.is_remote_server() || profile.is_containerized_runtime() {
                    needs_remote_line = true;
                }
            }
        }
    }

    if needs_hardware_line {
        if let Some(observation) = observations.iter().find(|item| {
            item.success
                && item.source_attribution.can_infer_user_device
                && item
                    .capability_tags
                    .iter()
                    .any(|tag| tag == "hardware_inspection" || tag == "gpu_inspection")
        }) {
            lines.push(format!("Local hardware evidence: {}", observation.summary));
        } else if profile.can_directly_inspect_user_hardware() {
            lines.push(
                "Bash and host inspection commands execute on the user's local machine in this session, so hardware/GPU/system outputs are local host facts.".into(),
            );
        }
    }

    if needs_workspace_line {
        if let Some(observation) = observations.iter().find(|item| {
            item.success
                && item.source_attribution.can_infer_user_device
                && item
                    .capability_tags
                    .iter()
                    .any(|tag| tag == "workspace_inspection")
        }) {
            lines.push(format!("Local workspace evidence: {}", observation.summary));
        } else if profile.can_directly_inspect_user_workspace() {
            lines.push(
                "File tools operate on the local workspace visible to this runtime in the current session.".into(),
            );
        }
    }

    if needs_external_line {
        if let Some(observation) = observations.iter().find(|item| {
            item.success && item.capability_tags.iter().any(|tag| tag == "external_web")
        }) {
            lines.push(format!("External web evidence: {}", observation.summary));
        } else {
            lines.push(
                "WebSearch/WebFetch results come from external web sources via Firecrawl, not from the user's machine.".into(),
            );
        }
    }

    if needs_remote_line {
        if let Some(observation) = observations.iter().find(|item| {
            item.success
                && item
                    .capability_tags
                    .iter()
                    .any(|tag| tag == "remote_execution" || tag == "container_execution")
        }) {
            lines.push(format!("Remote/runtime evidence: {}", observation.summary));
        } else {
            lines.push(
                "Remote or containerized results must be described as coming from that runtime, not as user-device facts.".into(),
            );
        }
    }

    if needs_no_bridge_line {
        lines.push(
            "This session does not currently expose direct user-device inspection unless a tool result explicitly sets can_infer_user_device=true.".into(),
        );
    }

    lines.sort();
    lines.dedup();
    lines
}

fn split_sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        current.push(ch);
        if matches!(ch, '.' | '!' | '?' | '\n') {
            let sentence = current.trim();
            if !sentence.is_empty() {
                out.push(sentence.to_string());
            }
            current.clear();
        }
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }
    out
}

fn preview_text(text: &str, max_chars: usize) -> String {
    let chars = text.chars().collect::<Vec<_>>();
    if chars.len() <= max_chars {
        text.trim().to_string()
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

fn normalize_text(text: &str) -> String {
    text.to_lowercase()
        .replace(['ă', 'â'], "a")
        .replace('î', "i")
        .replace(['ș', 'ş'], "s")
        .replace(['ț', 'ţ'], "t")
}

fn denies_hardware_access(text: &str) -> bool {
    contains_negative_access(
        text,
        &[
            "hardware",
            "gpu",
            "device",
            "host hardware",
            "sistem",
            "placa video",
            "placa grafica",
        ],
    )
}

fn denies_file_access(text: &str) -> bool {
    contains_negative_access(
        text,
        &[
            "your files",
            "your workspace",
            "local files",
            "local workspace",
            "fisierele tale",
            "workspace-ul tau",
            "workspace ul tau",
        ],
    )
}

fn contains_negative_access(text: &str, objects: &[&str]) -> bool {
    let negatives = [
        "do not have access",
        "don't have access",
        "cannot access",
        "can't access",
        "cannot inspect",
        "can't inspect",
        "cannot read",
        "can't read",
        "no access to",
        "nu am acces",
        "nu pot accesa",
        "nu pot inspecta",
        "nu pot citi",
        "nu pot verifica",
    ];
    negatives.iter().any(|neg| text.contains(neg))
        && objects.iter().any(|object| text.contains(object))
}

fn claims_remote_runtime(text: &str) -> bool {
    [
        "my server",
        "remote server",
        "serverul meu",
        "server remote",
        "ran on a server",
        "rulez pe un server",
        "a rulat pe un server",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

fn claims_local_runtime_or_user_machine_fact(text: &str) -> bool {
    claims_user_machine_verification(text)
        || claims_user_file_inspection(text)
        || claims_user_hardware_inspection(text)
        || [
            "running locally on your machine",
            "ran locally on your machine",
            "on your pc",
            "pe masina ta",
            "pe calculatorul tau",
            "local pe dispozitivul tau",
        ]
        .iter()
        .any(|needle| text.contains(needle))
}

fn claims_user_machine_verification(text: &str) -> bool {
    [
        "verified this on your machine",
        "checked this on your machine",
        "confirmed this on your machine",
        "verified on your device",
        "am verificat asta pe masina ta",
        "am verificat pe calculatorul tau",
        "am confirmat pe dispozitivul tau",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

fn claims_user_file_inspection(text: &str) -> bool {
    let verbs = [
        "i inspected",
        "i checked",
        "i read",
        "i listed",
        "am inspectat",
        "am verificat",
        "am citit",
        "am listat",
    ];
    let objects = [
        "your files",
        "your workspace",
        "local workspace",
        "fisierele tale",
        "workspace-ul tau",
        "workspace ul tau",
    ];
    verbs.iter().any(|verb| text.contains(verb))
        && objects.iter().any(|object| text.contains(object))
}

fn claims_user_hardware_inspection(text: &str) -> bool {
    let verbs = [
        "i inspected",
        "i checked",
        "i verified",
        "i measured",
        "am inspectat",
        "am verificat",
        "am masurat",
    ];
    let objects = [
        "your gpu",
        "your hardware",
        "your system",
        "gpu-ul tau",
        "hardware-ul tau",
        "sistemul tau",
    ];
    verbs.iter().any(|verb| text.contains(verb))
        && objects.iter().any(|object| text.contains(object))
}

fn dedupe_violations(violations: Vec<ConsistencyViolation>) -> Vec<ConsistencyViolation> {
    let mut out = Vec::new();
    for violation in violations {
        if !out
            .iter()
            .any(|existing: &ConsistencyViolation| existing.kind == violation.kind)
        {
            out.push(violation);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local_profile() -> RuntimeProfile {
        RuntimeProfile {
            runtime_mode: "local_user_machine".into(),
            host_identity: "user_local_host".into(),
            bash_executes_on: "user_local_host".into(),
            workspace_root: "/home/user/project".into(),
            workspace_filesystem_scope: "local_workspace".into(),
            bash_filesystem_scope: "user_local_host_with_workspace_default".into(),
            direct_hardware_access: true,
            web_access: "tool_mediated_firecrawl".into(),
            persistent_memory: false,
            conversation_storage: "local_chat_files".into(),
            notes: vec![],
        }
    }

    fn remote_profile() -> RuntimeProfile {
        RuntimeProfile {
            runtime_mode: "remote_server".into(),
            host_identity: "remote_server".into(),
            bash_executes_on: "remote_server".into(),
            workspace_root: "/srv/gnomef".into(),
            workspace_filesystem_scope: "remote_workspace".into(),
            bash_filesystem_scope: "remote_server_fs".into(),
            direct_hardware_access: false,
            web_access: "tool_mediated_firecrawl".into(),
            persistent_memory: false,
            conversation_storage: "remote_chat_files".into(),
            notes: vec![],
        }
    }

    fn local_bash_observation(command: &str, stdout: &str) -> ToolObservation {
        let mut args = Map::new();
        args.insert("command".into(), json!(command));
        ToolObservation::from_success(
            &local_profile(),
            "Bash",
            &args,
            &json!({
                "command": command,
                "cwd": "/home/user/project",
                "exit_code": 0,
                "stdout": stdout,
                "stderr": "",
            }),
        )
    }

    #[test]
    fn local_gpu_check_blocks_hardware_denial() {
        let profile = local_profile();
        let observations = vec![local_bash_observation(
            "nvidia-smi --query-gpu=temperature.gpu --format=csv,noheader",
            "43",
        )];
        let repaired = enforce_final_answer(
            "I do not have access to your hardware. This command ran on my server.",
            &profile,
            &observations,
        );
        assert!(!normalize_text(&repaired).contains("do not have access to your hardware"));
        assert!(!normalize_text(&repaired).contains("my server"));
        assert!(normalize_text(&repaired).contains("local hardware evidence"));
        assert!(normalize_text(&repaired).contains("nvidia-smi"));
    }

    #[test]
    fn local_workspace_listing_blocks_file_denial() {
        let profile = local_profile();
        let observations = vec![ToolObservation::from_success(
            &profile,
            "Glob",
            &Map::new(),
            &json!({
                "pattern": "src/*.rs",
                "base_path": "/home/user/project",
                "matches": ["src/main.rs", "src/tools.rs"],
                "truncated": false,
            }),
        )];
        let repaired = enforce_final_answer(
            "I cannot inspect your files in this environment.",
            &profile,
            &observations,
        );
        assert!(!normalize_text(&repaired).contains("cannot inspect your files"));
        assert!(normalize_text(&repaired).contains("local workspace evidence"));
    }

    #[test]
    fn local_hardware_inspection_uses_local_host_fact() {
        let profile = local_profile();
        let observations = vec![local_bash_observation("lscpu", "Model name: AMD Ryzen")];
        let repaired = enforce_final_answer(
            "I can't inspect your hardware from here.",
            &profile,
            &observations,
        );
        assert!(normalize_text(&repaired).contains("user's local machine"));
        assert!(normalize_text(&repaired).contains("local hardware evidence"));
    }

    #[test]
    fn external_web_article_summary_is_not_local() {
        let profile = local_profile();
        let mut args = Map::new();
        args.insert("url".into(), json!("https://example.com/article"));
        let observations = vec![ToolObservation::from_success(
            &profile,
            "WebFetch",
            &args,
            &json!({
                "url": "https://example.com/article",
                "title": "Example article",
                "content": "Article text",
            }),
        )];
        let repaired = enforce_final_answer(
            "I verified this on your machine from that article.",
            &profile,
            &observations,
        );
        assert!(!normalize_text(&repaired).contains("verified this on your machine"));
        assert!(normalize_text(&repaired).contains("external web evidence"));
    }

    #[test]
    fn remote_gpu_check_is_not_local_user_machine() {
        let profile = remote_profile();
        let mut args = Map::new();
        args.insert("command".into(), json!("nvidia-smi"));
        let observations = vec![ToolObservation::from_success(
            &profile,
            "Bash",
            &args,
            &json!({
                "command": "nvidia-smi",
                "cwd": "/srv/gnomef",
                "exit_code": 0,
                "stdout": "55",
                "stderr": "",
            }),
        )];
        let repaired = enforce_final_answer(
            "I checked your GPU locally on your PC.",
            &profile,
            &observations,
        );
        assert!(!normalize_text(&repaired).contains("checked your gpu locally on your pc"));
        assert!(normalize_text(&repaired).contains("remote server"));
        assert!(normalize_text(&repaired).contains("remote/runtime evidence"));
    }

    #[test]
    fn remote_no_file_bridge_blocks_local_file_claim() {
        let profile = remote_profile();
        let repaired = enforce_final_answer(
            "I inspected your files in the local workspace.",
            &profile,
            &[],
        );
        assert!(!normalize_text(&repaired).contains("inspected your files in the local workspace"));
        assert!(
            normalize_text(&repaired)
                .contains("does not currently expose direct user-device inspection")
        );
    }
}
